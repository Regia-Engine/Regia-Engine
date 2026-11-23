// SPDX-License-Identifier: MPL-2.0
//! Playing a [`Sequence`] and, optionally, a [`ShotSequence`] on one shared clock (ADR-1614, book
//! `agents/sequence-player.md`).
//!
//! - [`SequencePlayer`] is a component: one entity per playing sequence. It holds the clock, the
//!   end policy ([`EndPolicy`]), an optional start delay and the clock source ([`ClockSource`]).
//! - [`SequenceBindings`], on the same entity, maps each track's `target` name to an entity, by
//!   scene guid ([`EntityGuid`]) or directly. An unbound target is reported once and its tracks
//!   are skipped.
//! - [`PropertyAppliers`] is a resource mapping a track's `property` path to the function that
//!   writes the sampled value. The built-ins cover transforms, visibility, lights and the camera's
//!   field of view, and a host registers its own.
//! - [`MarkerReached`] messages fire for every marker a step of playback crosses. A step covers
//!   the half-open interval `(from, to]`; the first step after a start at time zero also covers
//!   zero. Markers of one step are ordered by time, then by name. Seeking never fires markers.
//! - [`SequenceEventFired`] messages fire for every key of an enabled event track
//!   ([`Channel::Event`], `.seq.ron` v6, ADR-8105) a step crosses, by the same rules: forward play
//!   only (once per pass of a loop), ordered by time, then track order, then file order; a
//!   [`SequencePlayer::skip_to`] delivers the keys it jumps over with `skipped` set; a seek, a
//!   scrub and reverse play (a backward external write is a seek) fire nothing. Only the
//!   sequence's own event tracks fire (not those of its sub-sequences).
//! - The shot list drives a camera through the takeover in [`super::takeover`], which blends from
//!   the gameplay pose into the shots and back.
//! - `.seq.ron` v2 (ADR-1880): clip tracks blend [`SequenceClips`] onto the joints of the rig
//!   their target binds; sub-sequence tracks play [`SequenceLibrary`] sequences on this clock
//!   (their values, clips, sounds and markers, with renamed targets); audio tracks start and stop
//!   [`regia_audio`] voices through [`super::audio`].
//!
//! # Clock
//!
//! The clock counts whole nanoseconds. On [`ClockSource::Gameplay`] it advances by the engine's
//! gameplay [`Time`] delta, so pause, time scale and the fixed steps of `play.step` apply and
//! nothing reads the wall clock. Because the count is an integer, `2N` steps of `d / 2` reach
//! exactly the instant `N` steps of `d` reach, so a replay at 60 Hz samples the same values and
//! fires the same markers as one at 30 Hz. On [`ClockSource::External`] the host (render-out)
//! writes the time with [`SequencePlayer::set_external_time`].
//!
//! # Example
//!
//! ```
//! use regia_framework::sequencer::{EndPolicy, Sequence, SequencePlayer};
//!
//! let sequence = Sequence::from_ron(r#"(version: 1,
//!     tracks: [(name: "move", target: "hero", property: "transform.position",
//!         keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 2.0, value: (4.0, 0.0, 0.0))]))],
//!     markers: [(name: "start", time: 0.0), (name: "half", time: 1.0)],
//! )"#).expect("valid");
//! let mut player = SequencePlayer::new(sequence);
//! player.end = EndPolicy::Once;
//! player.play();
//! player.step(std::time::Duration::from_millis(1500));
//! let fired: Vec<String> = player.take_fired().into_iter().map(|m| m.name).collect();
//! assert_eq!(fired, ["start", "half"]);
//! assert_eq!(player.time(), 1.5);
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3, det_quat_from_yaw_pitch_roll};
use regia_ecs::ecs::system::SystemParam;
use regia_render::{Camera, DirectionalLight, PointLight, SpotLight};
use regia_scene::Transform;
use regia_scene::components::{EntityGuid, Visibility};
use regia_scene::streaming::OriginOffset;

use std::sync::Arc;

use regia_animation::{AnimationClip, SkinnedMesh};

use super::asset::{Channel, Sequence};
use super::sample::{ChannelValue, ClipSample, FlatMarker, SequenceLibrary};
use crate::camera::CameraSet;
use crate::shots::{ShotSample, ShotSequence};

/// Most whole loops one step replays markers for. A step longer than this many loops (a huge
/// time-scale jump on a short looping sequence) fires the markers of the first loops and counts
/// the rest in [`SequencePlayer::skipped_cycles`].
pub const MAX_REPLAYED_CYCLES: u64 = 8;

/// Nanoseconds in a second.
const NANOS: f64 = 1.0e9;

/// Seconds to whole nanoseconds, rounded; negative and non-finite read as zero.
#[must_use]
pub fn seconds_to_nanos(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    let nanos = (seconds * NANOS).round();
    if nanos >= u64::MAX as f64 {
        u64::MAX
    } else {
        nanos as u64
    }
}

/// Whole nanoseconds to seconds.
#[must_use]
pub fn nanos_to_seconds(nanos: u64) -> f64 {
    nanos as f64 / NANOS
}

/// What a player does when its clock reaches the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EndPolicy {
    /// Stop on the last frame and keep it: values stay applied and the camera stays taken over
    /// until the player is stopped, cleared or despawned.
    #[default]
    Hold,
    /// Wrap to the start. Markers at the end fire before the wrap and markers at zero after it;
    /// each wrap is a camera cut. A zero-length player cannot wrap and finishes like
    /// [`Hold`](Self::Hold).
    Loop,
    /// Stop on the last frame, apply it once and let go: the camera blends back to gameplay.
    Once,
}

impl EndPolicy {
    /// The lowercase name the agent protocol uses (`hold`, `loop`, `once`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Loop => "loop",
            Self::Once => "once",
        }
    }

    /// Parses [`name`](Self::name).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "hold" => Some(Self::Hold),
            "loop" => Some(Self::Loop),
            "once" => Some(Self::Once),
            _ => None,
        }
    }
}

/// Where a player's clock comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ClockSource {
    /// The engine's gameplay [`Time`] delta (pause, time scale and `play.step` apply).
    #[default]
    Gameplay,
    /// Written by the host each frame with [`SequencePlayer::set_external_time`] (render-out).
    /// The start delay does not apply.
    External,
}

impl ClockSource {
    /// The lowercase name the agent protocol uses (`gameplay`, `external`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Gameplay => "gameplay",
            Self::External => "external",
        }
    }

    /// Parses [`name`](Self::name).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "gameplay" => Some(Self::Gameplay),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

/// Where a player is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    /// Loaded, not started (or stopped): nothing is applied and no camera is held.
    #[default]
    Stopped,
    /// Started, counting down the start delay. Nothing is applied yet.
    Waiting,
    /// The clock runs.
    Playing,
    /// The clock is held; values stay applied and the camera stays held.
    Paused,
    /// The clock reached the end under [`EndPolicy::Hold`] or [`EndPolicy::Once`].
    Finished,
}

impl PlaybackStatus {
    /// The lowercase name the agent protocol uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Waiting => "waiting",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Finished => "finished",
        }
    }
}

/// How a player's shot list takes over a camera ([`super::takeover`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TakeoverSettings {
    /// `false` keeps the shot list as data only: no camera is driven.
    pub enabled: bool,
    /// The camera entity to drive; `None` drives the camera with the highest `order` (ties go to
    /// the lowest entity id), the same rule as the shot player.
    pub camera: Option<Entity>,
    /// Seconds to blend from the live gameplay pose into the shots (`0` cuts).
    pub blend_in: f32,
    /// Seconds to blend from the shots back to the gameplay pose (`0` cuts).
    pub blend_out: f32,
}

impl Default for TakeoverSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            camera: None,
            blend_in: 0.5,
            blend_out: 0.5,
        }
    }
}

/// One marker a step crossed.
#[derive(Clone, Debug, PartialEq)]
pub struct FiredMarker {
    /// Index of the marker in the player's flattened marker list: a marker of the sequence
    /// itself has its [`Sequence::markers`] index; a sub-sequence marker comes after them
    /// ([`Sequence::flat_markers`]).
    pub index: usize,
    /// The marker's name (`track/name` for a sub-sequence marker).
    pub name: String,
    /// The marker's time, seconds.
    pub time: f64,
    /// The marker's payload.
    pub payload: String,
    /// How many times the clock had wrapped when it fired (`0` on the first pass).
    pub cycle: u32,
    /// `true` when a skip ([`SequencePlayer::skip_to`]) jumped over the marker: it is delivered
    /// so gameplay state stays right, but nothing was shown at its time (ADR-2948).
    pub skipped: bool,
}

/// One event key a step crossed (`.seq.ron` v6, ADR-8105): the event-track twin of
/// [`FiredMarker`] ([`SequencePlayer::take_fired_events`]).
#[derive(Clone, Debug, PartialEq)]
pub struct FiredEvent {
    /// The event track's name.
    pub track: String,
    /// The track's binding (`target`; empty for a sequence-wide event).
    pub target: String,
    /// The event name.
    pub event: String,
    /// The key's payload.
    pub payload: String,
    /// The key's time, seconds.
    pub time: f64,
    /// How many times the clock had wrapped when it fired (`0` on the first pass).
    pub cycle: u32,
    /// `true` when a skip ([`SequencePlayer::skip_to`]) jumped over the key.
    pub skipped: bool,
}

/// A problem a player reports once (and logs once) instead of failing the frame.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SequenceIssue {
    /// No binding (or a binding to an entity or guid that does not exist) for this target name;
    /// its tracks are skipped until it binds.
    UnboundTarget {
        /// The track target name.
        target: String,
    },
    /// No applier is registered for this property path; the track is skipped.
    UnknownProperty {
        /// The track name.
        track: String,
        /// The property path.
        property: String,
    },
    /// The applier refused the value (wrong channel kind, missing component).
    ApplyFailed {
        /// The track name.
        track: String,
        /// What the applier said.
        reason: String,
    },
    /// A clip track names a clip that is not in [`SequenceClips`]; that section is skipped.
    UnknownClip {
        /// The track name.
        track: String,
        /// The clip name.
        clip: String,
    },
    /// An audio track names a sound that is not in `regia_audio::SoundAssets`; that section is
    /// silent.
    UnknownSound {
        /// The track name (`[track, section, ...]` path in the tree).
        track: String,
        /// The sound name.
        sound: String,
    },
    /// The sub-sequence tree is incomplete or cyclic ([`SequenceLibrary::check`]); the missing
    /// or repeated parts are skipped.
    SubSequence {
        /// What the check said.
        reason: String,
    },
    /// A video section's source file does not exist; that section shows nothing and the rest of
    /// the sequence plays (kind `seq_video_missing`).
    VideoMissing {
        /// The track name.
        track: String,
        /// The source's project path.
        video: String,
    },
    /// A video section's source opened or decoded with an error; that section shows nothing or
    /// holds its last picture (kind `seq_video_failed`).
    VideoFailed {
        /// The track name.
        track: String,
        /// The source's project path.
        video: String,
        /// The media error code, or `seek_bound` when a seek outran its decode budget.
        code: String,
    },
    /// Video sections cannot play in this app (kind `seq_video_unavailable`).
    VideoUnavailable {
        /// The track name.
        track: String,
        /// What is missing.
        reason: String,
    },
    /// A spawnable's prefab did not load or spawn (kind `seq_spawn_failed`, ADR-2948): the
    /// binding stays unbound, so the tracks that target it are skipped.
    SpawnFailed {
        /// The spawnable's binding name.
        binding: String,
        /// The prefab reference.
        prefab: String,
        /// Why: `prefab_missing`, `prefab_read`, `prefab_parse`, `prefab_bake`,
        /// `prefab_not_registered` or `prefab_spawn`.
        code: String,
    },
}

impl SequenceIssue {
    /// The lowercase kind the agent protocol reports (`unbound_target`, `unknown_property`,
    /// `apply_failed`, `unknown_clip`, `unknown_sound`, `sub_sequence`, `seq_video_missing`,
    /// `seq_video_failed`, `seq_video_unavailable`, `seq_spawn_failed`).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UnboundTarget { .. } => "unbound_target",
            Self::UnknownProperty { .. } => "unknown_property",
            Self::ApplyFailed { .. } => "apply_failed",
            Self::UnknownClip { .. } => "unknown_clip",
            Self::UnknownSound { .. } => "unknown_sound",
            Self::SubSequence { .. } => "sub_sequence",
            Self::VideoMissing { .. } => "seq_video_missing",
            Self::VideoFailed { .. } => "seq_video_failed",
            Self::VideoUnavailable { .. } => "seq_video_unavailable",
            Self::SpawnFailed { .. } => "seq_spawn_failed",
        }
    }
}

impl std::fmt::Display for SequenceIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnboundTarget { target } => write!(f, "target `{target}` is not bound"),
            Self::UnknownProperty { track, property } => {
                write!(f, "track `{track}`: no applier for property `{property}`")
            }
            Self::ApplyFailed { track, reason } => write!(f, "track `{track}`: {reason}"),
            Self::UnknownClip { track, clip } => {
                write!(f, "track `{track}`: no clip `{clip}` in SequenceClips")
            }
            Self::UnknownSound { track, sound } => {
                write!(f, "track `{track}`: no sound `{sound}` in SoundAssets")
            }
            Self::SubSequence { reason } => write!(f, "{reason}"),
            Self::VideoMissing { track, video } => {
                write!(f, "track `{track}`: video `{video}` does not exist")
            }
            Self::VideoFailed { track, video, code } => {
                write!(f, "track `{track}`: video `{video}` failed: {code}")
            }
            Self::VideoUnavailable { track, reason } => write!(f, "track `{track}`: {reason}"),
            Self::SpawnFailed {
                binding,
                prefab,
                code,
            } => {
                write!(f, "spawnable `{binding}`: prefab `{prefab}` failed: {code}")
            }
        }
    }
}

/// Plays one [`Sequence`] and, optionally, one [`ShotSequence`] on a shared clock. Spawn one entity
/// per sequence; add [`SequenceBindings`] to the same entity and the [`SequencerPlugin`].
///
/// The public fields are settings and take effect on the next frame; the clock and the status are
/// changed through the methods.
#[derive(Component, Debug, Clone)]
pub struct SequencePlayer {
    /// What happens at the end.
    pub end: EndPolicy,
    /// Seconds between [`play`](Self::play) and the clock starting (gameplay clock only).
    pub start_delay: f64,
    /// Where the clock comes from.
    pub clock: ClockSource,
    /// How the shot list takes over a camera.
    pub takeover: TakeoverSettings,
    /// `true` keeps the audio tracks silent while the clock runs. Render-out sets it for the
    /// frames before a job's frame 0, whose sound it throws away.
    pub hold_audio: bool,
    /// `true` writes back, when the player stops or lets go at the end, the value each keyed
    /// track's property had before the player first wrote it (ADR-2948). Starts as the sequence's
    /// own `restore_on_stop`; [`replace_sequence`](Self::replace_sequence) resets it from there.
    pub restore_on_stop: bool,
    sequence: Sequence,
    shots: Option<ShotSequence>,
    duration_ns: u64,
    shots_duration_ns: u64,
    clock_ns: u64,
    delay_left_ns: u64,
    status: PlaybackStatus,
    include_start: bool,
    cycle: u32,
    skipped_cycles: u64,
    start_requested: bool,
    started: Option<u64>,
    fired: Vec<FiredMarker>,
    markers_fired: u64,
    just_finished: bool,
    finish_reported: bool,
    apply_now: bool,
    shot: Option<ShotSample>,
    shot_changed: bool,
    wrapped: bool,
    cuts: u64,
    resolved: Vec<Option<Entity>>,
    issues: BTreeSet<SequenceIssue>,
    pub(crate) held_camera: Option<(Entity, f32)>,
    finish_overshoot_ns: u64,
    release_ns: Option<u64>,
    acquire_ns: Option<u64>,
    external_ns: u64,
    /// Whether the shots were visible before the first forward external write since the last
    /// advance (`None`: no such write). The host writes an external clock before
    /// `SequencerSet::Advance`, so the advance would otherwise see the shots already gone and
    /// never set `release_ns` (audit-seq Medium).
    external_showed: Option<bool>,
    flat_markers: Vec<FlatMarker>,
    flat_markers_ns: Vec<u64>,
    library: Option<Arc<SequenceLibrary>>,
    library_revision: Option<u64>,
    targets: Vec<String>,
    has_video: bool,
    target_entities: BTreeMap<String, Entity>,
    jumps: u64,
    /// Bumped when the sequence value is replaced, so a warmed frame is not reused.
    content_gen: u64,
    /// `(clock_ns, content_gen, library revision)` of `frame_cache`.
    frame_key: Option<(u64, u64, Option<u64>)>,
    /// The frame `warm_frame` sampled for `frame_key`.
    frame_cache: Option<CachedFrame>,
    /// Started and waiting for its spawnables' prefabs: the clock (and the start delay) hold.
    preroll: bool,
    /// A skip asked for during the pre-roll, applied when it ends.
    pending_skip: Option<u64>,
    /// The binding names of the sequence's spawnables, sorted (never reported unbound).
    spawn_names: Vec<String>,
    /// Markers delivered with `skipped` since the player was created.
    markers_skipped: u64,
    /// Every key of the sequence's enabled event tracks, ordered by time, then track, then file
    /// order (v6, ADR-8105).
    flat_events: Vec<FiredEvent>,
    flat_events_ns: Vec<u64>,
    /// The event keys collected since the last [`take_fired_events`](Self::take_fired_events).
    fired_events: Vec<FiredEvent>,
    /// Event keys fired since the player was created.
    events_fired: u64,
}

impl SequencePlayer {
    /// A stopped player at time zero, with the default settings (hold at the end, no start delay,
    /// gameplay clock, takeover on with half-second blends).
    #[must_use]
    pub fn new(sequence: Sequence) -> Self {
        let mut player = Self {
            end: EndPolicy::default(),
            start_delay: 0.0,
            clock: ClockSource::default(),
            takeover: TakeoverSettings::default(),
            hold_audio: false,
            restore_on_stop: sequence.restore_on_stop,
            sequence,
            shots: None,
            duration_ns: 0,
            shots_duration_ns: 0,
            clock_ns: 0,
            delay_left_ns: 0,
            status: PlaybackStatus::Stopped,
            include_start: false,
            cycle: 0,
            skipped_cycles: 0,
            start_requested: false,
            started: None,
            fired: Vec::new(),
            markers_fired: 0,
            just_finished: false,
            finish_reported: false,
            apply_now: false,
            shot: None,
            shot_changed: false,
            wrapped: false,
            cuts: 0,
            resolved: Vec::new(),
            issues: BTreeSet::new(),
            held_camera: None,
            finish_overshoot_ns: 0,
            release_ns: None,
            acquire_ns: None,
            external_ns: 0,
            external_showed: None,
            flat_markers: Vec::new(),
            flat_markers_ns: Vec::new(),
            library: None,
            library_revision: None,
            targets: Vec::new(),
            has_video: false,
            target_entities: BTreeMap::new(),
            jumps: 0,
            content_gen: 0,
            frame_key: None,
            frame_cache: None,
            preroll: false,
            pending_skip: None,
            spawn_names: Vec::new(),
            markers_skipped: 0,
            flat_events: Vec::new(),
            flat_events_ns: Vec::new(),
            fired_events: Vec::new(),
            events_fired: 0,
        };
        player.refresh_duration();
        player
    }

    /// The same player with a shot list on its clock.
    #[must_use]
    pub fn with_shots(mut self, shots: ShotSequence) -> Self {
        self.set_shots(Some(shots));
        self
    }

    /// The sequence.
    #[must_use]
    pub fn sequence(&self) -> &Sequence {
        &self.sequence
    }

    /// The shot list, if any.
    #[must_use]
    pub fn shots(&self) -> Option<&ShotSequence> {
        self.shots.as_ref()
    }

    /// Swaps in an edited sequence, keeping the clock (clamped to the new length) and the status.
    /// Reported issues are forgotten so they are checked again.
    pub fn replace_sequence(&mut self, sequence: Sequence) {
        self.restore_on_stop = sequence.restore_on_stop;
        self.sequence = sequence;
        self.content_gen = self.content_gen.wrapping_add(1);
        self.frame_key = None;
        self.frame_cache = None;
        self.issues.clear();
        self.check_tree();
        self.refresh_duration();
    }

    /// Follows the sub-sequence tracks through `library` (a snapshot: later changes to the
    /// resource reach the player through the [`SequencerPlugin`], which calls this when the
    /// [`SequenceLibrary`] resource's revision changes). An incomplete or cyclic tree is reported
    /// once ([`SequenceIssue::SubSequence`]) and the missing parts are skipped.
    pub fn set_library(&mut self, library: &SequenceLibrary) {
        self.library_revision = Some(library.revision());
        self.library = Some(Arc::new(library.clone()));
        self.frame_key = None;
        self.frame_cache = None;
        self.issues
            .retain(|issue| !matches!(issue, SequenceIssue::SubSequence { .. }));
        self.check_tree();
        self.refresh_tree();
    }

    /// Reports the first problem of the sub-sequence tree against the library the player follows.
    fn check_tree(&mut self) {
        let problem = self
            .library
            .as_deref()
            .and_then(|library| library.check(&self.sequence).err());
        if let Some(error) = problem {
            self.report(SequenceIssue::SubSequence {
                reason: error.to_string(),
            });
        }
    }

    /// `true` when an enabled track of the sequence is a sub-sequence track.
    #[must_use]
    pub fn has_sub_sequences(&self) -> bool {
        self.sequence
            .tracks
            .iter()
            .any(|t| t.enabled && matches!(t.keys, Channel::Sub(_)))
    }

    /// The sub-sequence library the player follows (`None` before [`set_library`](Self::set_library)).
    #[must_use]
    pub fn library(&self) -> Option<&SequenceLibrary> {
        self.library.as_deref()
    }

    /// Every marker the player fires, its own and its sub-sequences', on its clock.
    #[must_use]
    pub fn flat_markers(&self) -> &[FlatMarker] {
        &self.flat_markers
    }

    /// The target names the player binds (its keyed and clip tracks, and its sub-sequences'
    /// after renames), sorted.
    #[must_use]
    pub fn target_names(&self) -> &[String] {
        &self.targets
    }

    /// The entity each target name resolved to on the last frame (unbound names are missing).
    #[must_use]
    pub fn target_entities(&self) -> &BTreeMap<String, Entity> {
        &self.target_entities
    }

    /// `true` when an enabled track of the tree holds video sections: what the video sync runs for.
    #[must_use]
    pub fn has_video_sections(&self) -> bool {
        self.has_video
    }

    /// Counts the clock's jumps (a seek, a restart, a loop wrap, a stop): what audio voices
    /// restart on.
    #[must_use]
    pub fn jumps(&self) -> u64 {
        self.jumps
    }

    /// The sequence sampled at the clock, sub-sequences followed through the player's library.
    ///
    /// After the advance step warms this frame, later reads in the same frame return that
    /// sample. A direct call with no warm sample still samples live.
    #[must_use]
    pub fn frame(&self) -> super::sample::SequenceFrame<'_> {
        let key = (self.clock_ns, self.content_gen, self.library_revision);
        if self.frame_key == Some(key)
            && let Some(cached) = &self.frame_cache
        {
            return cached.view();
        }
        self.sample_now()
    }

    fn sample_now(&self) -> super::sample::SequenceFrame<'_> {
        match self.library.as_deref() {
            Some(library) => self.sequence.sample_with(self.time(), library),
            None => self.sequence.sample(self.time()),
        }
    }

    /// Samples the clock once so apply, audio and subtitles share that frame.
    fn warm_frame(&mut self) {
        let key = (self.clock_ns, self.content_gen, self.library_revision);
        if self.frame_key == Some(key) && self.frame_cache.is_some() {
            return;
        }
        let owned = CachedFrame::capture(self);
        self.frame_cache = Some(owned);
        self.frame_key = Some(key);
    }

    fn refresh_tree(&mut self) {
        let library = self.library.as_deref();
        self.has_video = self.sequence.has_video_sections(library);
        self.flat_markers = self.sequence.flat_markers(library);
        self.flat_markers_ns = self
            .flat_markers
            .iter()
            .map(|m| seconds_to_nanos(m.time))
            .collect();
        self.targets = self.sequence.target_names(library);
        self.refresh_events();
        let mut spawn_names: Vec<String> = self
            .sequence
            .spawnables
            .iter()
            .map(|s| s.binding.clone())
            .collect();
        spawn_names.sort_unstable();
        self.spawn_names = spawn_names;
    }

    /// The binding names the sequence spawns (v5 spawnables), sorted.
    #[must_use]
    pub fn spawn_names(&self) -> &[String] {
        &self.spawn_names
    }

    /// `true` while the player was started and waits for its spawnables' prefabs to load (the
    /// pre-roll, ADR-2948): its status is [`PlaybackStatus::Waiting`], nothing is applied and the
    /// clock and the start delay hold.
    #[must_use]
    pub fn is_prerolling(&self) -> bool {
        self.preroll
    }

    /// Ends the pre-roll (the spawnable system calls this once every prefab loaded or failed).
    pub(crate) fn end_preroll(&mut self) {
        if self.preroll {
            self.preroll = false;
            if self.status == PlaybackStatus::Waiting && self.delay_left_ns == 0 {
                self.status = PlaybackStatus::Playing;
            }
            if let Some(at) = self.pending_skip.take() {
                self.skip_now(at);
            }
        }
    }

    /// Markers delivered with `skipped` set since the player was created.
    #[must_use]
    pub fn markers_skipped(&self) -> u64 {
        self.markers_skipped
    }

    /// Points `name` at a spawned entity (or unbinds it) for this frame's apply: the spawnable
    /// system calls this after the advance resolved the hand-made bindings.
    pub(crate) fn bind_spawned(&mut self, name: &str, entity: Option<Entity>) {
        for (track, slot) in self.sequence.tracks.iter().zip(self.resolved.iter_mut()) {
            if track.target == name
                && !matches!(
                    track.keys,
                    Channel::Audio(_) | Channel::Sub(_) | Channel::Video(_) | Channel::Event(_)
                )
            {
                *slot = entity;
            }
        }
        match entity {
            Some(entity) if self.targets.iter().any(|t| t == name) => {
                self.target_entities.insert(name.to_string(), entity);
            }
            _ => {
                self.target_entities.remove(name);
            }
        }
    }

    /// Sets or removes the shot list, keeping the clock (clamped to the new length).
    pub fn set_shots(&mut self, shots: Option<ShotSequence>) {
        self.shots = shots;
        self.refresh_duration();
        self.resample();
    }

    fn refresh_duration(&mut self) {
        self.refresh_tree();
        self.shots_duration_ns = self.shots.as_ref().map_or(0, |s| seconds_to_nanos(s.duration()));
        self.duration_ns = seconds_to_nanos(self.sequence.duration()).max(self.shots_duration_ns);
        self.clock_ns = self.clock_ns.min(self.duration_ns);
        self.apply_now = true;
    }

    /// Length in seconds: the longer of the sequence and the shot list.
    #[must_use]
    pub fn duration(&self) -> f64 {
        nanos_to_seconds(self.duration_ns)
    }

    /// The clock, seconds.
    #[must_use]
    pub fn time(&self) -> f64 {
        nanos_to_seconds(self.clock_ns)
    }

    /// The clock, whole nanoseconds.
    #[must_use]
    pub fn time_nanos(&self) -> u64 {
        self.clock_ns
    }

    /// Seconds of start delay still to wait.
    #[must_use]
    pub fn delay_left(&self) -> f64 {
        nanos_to_seconds(self.delay_left_ns)
    }

    /// Where the player is in its life.
    #[must_use]
    pub fn status(&self) -> PlaybackStatus {
        self.status
    }

    /// How many times the clock has wrapped ([`EndPolicy::Loop`]).
    #[must_use]
    pub fn cycle(&self) -> u32 {
        self.cycle
    }

    /// Whole loops a single step skipped without firing their markers ([`MAX_REPLAYED_CYCLES`]).
    #[must_use]
    pub fn skipped_cycles(&self) -> u64 {
        self.skipped_cycles
    }

    /// Markers fired since the player was created.
    #[must_use]
    pub fn markers_fired(&self) -> u64 {
        self.markers_fired
    }

    /// The order this player was started in (larger is more recent); `None` before the first
    /// frame after [`play`](Self::play). The most recently started player wins a contested camera.
    #[must_use]
    pub fn start_stamp(&self) -> Option<u64> {
        self.started
    }

    /// The shot sample at the clock (`None` without a shot list).
    #[must_use]
    pub fn shot_sample(&self) -> Option<&ShotSample> {
        self.shot.as_ref()
    }

    /// Shot changes counted since the player was created: a change of shot, a wrap, and any jump of
    /// the clock (a seek, a restart, a stop) while there are shots. The takeover turns the visible
    /// ones into renderer camera cuts.
    #[must_use]
    pub fn shot_changes(&self) -> u64 {
        self.cuts
    }

    /// `true` when a step or seek moved to another shot (or wrapped) since the last
    /// [`take_shot_change`](Self::take_shot_change).
    #[must_use]
    pub fn shot_changed(&self) -> bool {
        self.shot_changed
    }

    /// Reads and clears [`shot_changed`](Self::shot_changed) (the takeover calls this once a
    /// frame).
    pub fn take_shot_change(&mut self) -> bool {
        std::mem::take(&mut self.shot_changed)
    }

    /// The problems reported so far (each once), sorted.
    pub fn issues(&self) -> impl Iterator<Item = &SequenceIssue> {
        self.issues.iter()
    }

    /// The entity each track's target resolved to on the last frame, by track index (`None`:
    /// unbound).
    #[must_use]
    pub fn resolved_targets(&self) -> &[Option<Entity>] {
        &self.resolved
    }

    /// The camera this player's shots hold and the blend progress there (`0..=1`), as of the last
    /// frame; `None` when it holds none (no active shot, or a more recent player won).
    #[must_use]
    pub fn held_camera(&self) -> Option<(Entity, f32)> {
        self.held_camera
    }

    /// On the frame the clock carried the shots out of view (the end under [`EndPolicy::Once`], or
    /// past the end of a shot list shorter than the sequence): how much of that frame's step came
    /// after the instant they ended, so the camera's blend out starts there and not at the frame
    /// boundary. `None` on every other frame, and when the shots stopped by a call between frames.
    #[must_use]
    pub fn release_overshoot(&self) -> Option<Duration> {
        self.release_ns.map(Duration::from_nanos)
    }

    /// On the frame the clock carried the shots into view (a start delay ending mid-step, a loop
    /// wrapping back into a shorter shot list): how long they have been visible, so the camera's
    /// blend in starts at that instant. `None` on every other frame.
    #[must_use]
    pub fn acquire_offset(&self) -> Option<Duration> {
        self.acquire_ns.map(Duration::from_nanos)
    }

    /// `true` while the player holds its outputs: values applied each frame and the camera held.
    /// Playing, paused, or finished under [`EndPolicy::Hold`].
    #[must_use]
    pub fn is_active(&self) -> bool {
        match self.status {
            PlaybackStatus::Playing => true,
            // Paused during the start delay: the clock never started, so nothing shows yet.
            PlaybackStatus::Paused => self.delay_left_ns == 0,
            PlaybackStatus::Finished => self.end == EndPolicy::Hold,
            PlaybackStatus::Stopped | PlaybackStatus::Waiting => false,
        }
    }

    /// [`wants_apply`](Self::wants_apply) for the spawnable sync.
    pub(crate) fn holds_outputs(&self) -> bool {
        self.wants_apply()
    }

    /// `false` while a frame's values wait to be written (a skip or the end reached since the last
    /// apply): the cutscene hands back only once the last frame is on screen.
    pub(crate) fn frame_applied(&self) -> bool {
        !self.apply_now
    }

    /// `true` between [`play`](Self::play) and the next advance (the spawnable pre-roll retries
    /// failed loads on a fresh start).
    pub(crate) fn start_pending(&self) -> bool {
        self.start_requested
    }

    /// `true` when this frame's values should be written: while active, and once on the frame a
    /// [`EndPolicy::Once`] player finishes.
    fn wants_apply(&self) -> bool {
        self.is_active() || (self.apply_now && self.status == PlaybackStatus::Finished)
    }

    /// The shot sample the takeover should show: the player is active and the clock is inside the
    /// shot list (at its end only when the shot list is what ends the player). `None` lets the
    /// camera fall back to the next player or to gameplay.
    #[must_use]
    pub fn active_shot(&self) -> Option<&ShotSample> {
        if !self.is_active() || !self.takeover.enabled {
            return None;
        }
        let inside = self.clock_ns < self.shots_duration_ns
            || (self.clock_ns == self.shots_duration_ns && self.clock_ns >= self.duration_ns);
        if inside { self.shot.as_ref() } else { None }
    }

    /// Starts the player. From stopped or finished it starts at zero (after the start delay, on
    /// the gameplay clock) and the markers at zero fire on the first step; it also counts as the
    /// most recent start for the camera. From paused it resumes where it was.
    pub fn play(&mut self) {
        match self.status {
            PlaybackStatus::Playing | PlaybackStatus::Waiting => {}
            PlaybackStatus::Paused => {
                self.status = if self.delay_left_ns > 0 || self.preroll {
                    PlaybackStatus::Waiting
                } else {
                    PlaybackStatus::Playing
                };
            }
            PlaybackStatus::Stopped | PlaybackStatus::Finished => {
                if self.status == PlaybackStatus::Finished && self.clock_ns != 0 {
                    self.clock_ns = 0;
                    self.wrapped = true;
                }
                self.external_ns = self.clock_ns;
                self.cycle = 0;
                self.include_start = self.clock_ns == 0;
                self.delay_left_ns = if self.clock == ClockSource::Gameplay {
                    seconds_to_nanos(self.start_delay)
                } else {
                    0
                };
                // Spawnables load before the clock starts (the pre-roll, ADR-2948).
                self.preroll = !self.sequence.spawnables.is_empty();
                self.status = if self.delay_left_ns > 0 || self.preroll {
                    PlaybackStatus::Waiting
                } else {
                    PlaybackStatus::Playing
                };
                self.start_requested = true;
                self.finish_reported = false;
                self.apply_now = true;
                self.resample();
            }
        }
    }

    /// Starts from `time` seconds. From stopped or finished this is a fresh start like
    /// [`play`](Self::play) (start stamp, start delay, `SequenceFinished` again at the end; the
    /// markers at zero fire only when `time` is zero); from paused or playing it seeks and resumes.
    pub fn play_from(&mut self, time: f64) {
        match self.status {
            PlaybackStatus::Stopped | PlaybackStatus::Finished => {
                let at = seconds_to_nanos(time).min(self.duration_ns);
                if at != self.clock_ns {
                    self.wrapped = true;
                }
                self.clock_ns = at;
                self.status = PlaybackStatus::Stopped;
                self.play();
            }
            PlaybackStatus::Paused | PlaybackStatus::Playing | PlaybackStatus::Waiting => {
                self.seek(time);
                self.play();
            }
        }
    }

    /// Holds the clock where it is (a waiting player keeps its remaining delay).
    pub fn pause(&mut self) {
        if matches!(self.status, PlaybackStatus::Playing | PlaybackStatus::Waiting) {
            self.status = PlaybackStatus::Paused;
        }
    }

    /// Stops and returns to zero: nothing is applied and the camera is released.
    pub fn stop(&mut self) {
        self.status = PlaybackStatus::Stopped;
        if self.clock_ns != 0 {
            self.wrapped = true;
        }
        self.clock_ns = 0;
        self.external_ns = 0;
        self.delay_left_ns = 0;
        self.preroll = false;
        self.pending_skip = None;
        self.include_start = false;
        self.cycle = 0;
        self.resample();
    }

    /// Moves the clock to `time` seconds (clamped to the player's length). Fires no markers. A
    /// finished player becomes paused at the new time, so the instant shows.
    pub fn seek(&mut self, time: f64) {
        let at = seconds_to_nanos(time).min(self.duration_ns);
        if at != self.clock_ns {
            // A jump of the clock is a camera cut even inside one shot.
            self.wrapped = true;
        }
        self.clock_ns = at;
        self.external_ns = at;
        self.include_start = false;
        if self.status == PlaybackStatus::Finished {
            self.status = PlaybackStatus::Paused;
            // Reaching the end again sends `SequenceFinished` again.
            self.finish_reported = false;
        }
        self.apply_now = true;
        self.resample();
    }

    /// Advances a playing (or waiting) gameplay clock by `delta`, collecting the markers it crosses
    /// ([`take_fired`](Self::take_fired)). The [`SequencerPlugin`] calls this each frame with the
    /// gameplay delta; a host driving a player by hand calls it itself. Does nothing on an
    /// external clock.
    pub fn step(&mut self, delta: Duration) {
        if self.clock != ClockSource::Gameplay {
            return;
        }
        let nanos = u64::try_from(delta.as_nanos()).unwrap_or(u64::MAX);
        self.advance_nanos(nanos);
        self.resample();
    }

    /// Sets an external clock ([`ClockSource::External`]) to `time` seconds of the host's own
    /// (absolute) time. A forward move advances by the difference from the last written time, so it
    /// fires the markers crossed and follows the end policy (a loop wraps once per pass, not on every
    /// write); a backward move is a seek. Does nothing on the gameplay clock or while the player is
    /// not playing.
    pub fn set_external_time(&mut self, time: f64) {
        if self.clock != ClockSource::External || self.status != PlaybackStatus::Playing {
            return;
        }
        let to = seconds_to_nanos(time);
        if to >= self.external_ns {
            // The first forward write since the last advance: remember what was on screen, and
            // let this write's own finishing overshoot reach the release.
            if self.external_showed.is_none() {
                self.external_showed = Some(self.active_shot().is_some());
                self.finish_overshoot_ns = 0;
            }
            let delta = to - self.external_ns;
            self.external_ns = to;
            self.advance_nanos(delta);
            self.resample();
        } else {
            self.seek(time);
        }
    }

    /// The markers collected since the last call, in firing order.
    pub fn take_fired(&mut self) -> Vec<FiredMarker> {
        std::mem::take(&mut self.fired)
    }

    /// The event keys collected since the last call, in firing order (v6, ADR-8105). The
    /// [`SequencerPlugin`] turns them into [`SequenceEventFired`] messages; a host driving a
    /// player by hand reads them here.
    pub fn take_fired_events(&mut self) -> Vec<FiredEvent> {
        std::mem::take(&mut self.fired_events)
    }

    /// Event keys fired since the player was created (skipped ones included).
    #[must_use]
    pub fn events_fired(&self) -> u64 {
        self.events_fired
    }

    /// Every key of the sequence's enabled event tracks, in firing order.
    fn refresh_events(&mut self) {
        let mut events: Vec<(u64, FiredEvent)> = Vec::new();
        for track in self.sequence.tracks.iter().filter(|t| t.enabled) {
            let Channel::Event(keys) = &track.keys else {
                continue;
            };
            for key in keys {
                let time = f64::from(key.time);
                events.push((
                    seconds_to_nanos(time),
                    FiredEvent {
                        track: track.name.clone(),
                        target: track.target.clone(),
                        event: key.event.clone(),
                        payload: key.payload.clone(),
                        time,
                        cycle: 0,
                        skipped: false,
                    },
                ));
            }
        }
        // Stable: equal times keep track order, then file order.
        events.sort_by_key(|(ns, _)| *ns);
        self.flat_events_ns = events.iter().map(|(ns, _)| *ns).collect();
        self.flat_events = events.into_iter().map(|(_, e)| e).collect();
    }

    /// Appends the event keys in `(from, to]` (`[from, to]` with `include_from`).
    fn collect_events(&mut self, from: u64, to: u64, include_from: bool) {
        if self.flat_events.is_empty() {
            return;
        }
        let times = &self.flat_events_ns;
        let start = if include_from {
            times.partition_point(|&at| at < from)
        } else {
            times.partition_point(|&at| at <= from)
        };
        let end = times.partition_point(|&at| at <= to).max(start);
        let cycle = self.cycle;
        self.fired_events.extend(
            self.flat_events[start..end]
                .iter()
                .map(|e| FiredEvent { cycle, ..e.clone() }),
        );
        self.events_fired = self.events_fired.saturating_add((end - start) as u64);
    }

    /// Skips forward to `time` seconds (clamped to the player's length; a time behind the clock
    /// does nothing): the clock jumps there like a seek, but every marker in between is
    /// delivered, in the order playback would have fired it, with
    /// [`skipped`](FiredMarker::skipped) set, so gameplay that counts on markers ends in the same
    /// state as playing through (ADR-2948). Reaching the end finishes the player as playback
    /// would (a [`EndPolicy::Loop`] player finishes too: a skip never wraps), sends
    /// `SequenceFinished`, and applies the last frame. Works from playing, paused and waiting (the
    /// start delay is dropped); a stopped or finished player is left alone. During the pre-roll
    /// the skip waits for it: it is applied on the frame the pre-roll ends.
    pub fn skip_to(&mut self, time: f64) {
        if matches!(self.status, PlaybackStatus::Stopped | PlaybackStatus::Finished) {
            return;
        }
        // `seconds_to_nanos` reads a non-finite time as zero; an infinite skip means the end.
        let at = if time == f64::INFINITY {
            self.duration_ns
        } else {
            seconds_to_nanos(time).min(self.duration_ns)
        };
        if self.preroll {
            self.pending_skip = Some(self.pending_skip.map_or(at, |p| p.max(at)));
            return;
        }
        self.skip_now(at);
    }

    /// Skips to the end ([`skip_to`](Self::skip_to) with the player's length).
    pub fn skip_to_end(&mut self) {
        self.skip_to(f64::INFINITY);
    }

    fn skip_now(&mut self, at: u64) {
        if at < self.clock_ns {
            return;
        }
        if self.status == PlaybackStatus::Waiting {
            self.delay_left_ns = 0;
            self.status = PlaybackStatus::Playing;
        }
        // Paused during the start delay: the skip drops what is left of it too.
        self.delay_left_ns = 0;
        let from = self.clock_ns;
        let include = std::mem::take(&mut self.include_start);
        let first = self.fired.len();
        let first_event = self.fired_events.len();
        self.collect(from, at, include);
        for marker in &mut self.fired[first..] {
            marker.skipped = true;
        }
        for event in &mut self.fired_events[first_event..] {
            event.skipped = true;
        }
        self.markers_skipped = self
            .markers_skipped
            .saturating_add((self.fired.len() - first) as u64);
        if at != self.clock_ns {
            self.wrapped = true;
        }
        self.clock_ns = at;
        self.external_ns = at;
        self.apply_now = true;
        if at >= self.duration_ns {
            self.finish_overshoot_ns = 0;
            self.status = PlaybackStatus::Finished;
            self.just_finished = true;
        }
        self.resample();
    }

    fn advance_nanos(&mut self, delta: u64) {
        let mut delta = delta;
        if self.preroll {
            return;
        }
        if self.status == PlaybackStatus::Waiting {
            if delta < self.delay_left_ns {
                self.delay_left_ns -= delta;
                return;
            }
            delta -= self.delay_left_ns;
            self.delay_left_ns = 0;
            self.status = PlaybackStatus::Playing;
        }
        if self.status != PlaybackStatus::Playing {
            return;
        }
        let duration = self.duration_ns;
        let from = self.clock_ns;
        let include = std::mem::take(&mut self.include_start);
        let to = from.saturating_add(delta);
        self.apply_now = true;
        if to < duration {
            self.collect(from, to, include);
            self.clock_ns = to;
            return;
        }
        self.collect(from, duration, include);
        if self.end == EndPolicy::Loop && duration > 0 {
            let rest = to - duration;
            self.cycle = self.cycle.saturating_add(1);
            self.wrapped = true;
            let full = rest / duration;
            let replayed = full.min(MAX_REPLAYED_CYCLES);
            for _ in 0..replayed {
                self.collect(0, duration, true);
                self.cycle = self.cycle.saturating_add(1);
            }
            let skipped = full - replayed;
            self.skipped_cycles = self.skipped_cycles.saturating_add(skipped);
            self.cycle = self
                .cycle
                .saturating_add(u32::try_from(skipped).unwrap_or(u32::MAX));
            let rem = rest % duration;
            self.collect(0, rem, true);
            self.clock_ns = rem;
        } else {
            self.finish_overshoot_ns = to - duration;
            self.clock_ns = duration;
            self.status = PlaybackStatus::Finished;
            self.just_finished = true;
        }
    }

    /// Appends the markers in `(from, to]` (`[from, to]` with `include_from`), ordered by time
    /// then name, and the event keys of the same window. The marker list is already sorted, so
    /// the window is a binary search.
    fn collect(&mut self, from: u64, to: u64, include_from: bool) {
        self.collect_events(from, to, include_from);
        let first = self.fired.len();
        let times = &self.flat_markers_ns;
        let start = if include_from {
            times.partition_point(|&at| {
                #[cfg(test)]
                hot_counts::add_marker();
                at < from
            })
        } else {
            times.partition_point(|&at| {
                #[cfg(test)]
                hot_counts::add_marker();
                at <= from
            })
        };
        let end = times.partition_point(|&at| {
            #[cfg(test)]
            hot_counts::add_marker();
            at <= to
        });
        let end = end.max(start);
        for (marker, _) in self.flat_markers[start..end]
            .iter()
            .zip(&self.flat_markers_ns[start..end])
        {
            self.fired.push(FiredMarker {
                index: marker.index,
                name: marker.name.clone(),
                time: marker.time,
                payload: marker.payload.clone(),
                cycle: self.cycle,
                skipped: false,
            });
        }
        self.fired[first..].sort_by(|a, b| {
            seconds_to_nanos(a.time)
                .cmp(&seconds_to_nanos(b.time))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.markers_fired = self
            .markers_fired
            .saturating_add((self.fired.len() - first) as u64);
    }

    /// Re-samples the shot list at the clock and notes a shot change.
    fn resample(&mut self) {
        let time = nanos_to_seconds(self.clock_ns);
        let sample = self.shots.as_ref().and_then(|s| s.sample(time));
        let changed = match (&self.shot, &sample) {
            (Some(old), Some(new)) => old.shot != new.shot || self.wrapped,
            _ => false,
        };
        if self.wrapped {
            self.jumps = self.jumps.saturating_add(1);
        }
        self.wrapped = false;
        if changed {
            self.cuts = self.cuts.saturating_add(1);
            self.shot_changed = true;
        }
        self.shot = sample;
    }

    /// Forgets what was reported about a video section's source: it opened after all.
    #[cfg(feature = "media")]
    pub(crate) fn forget_video_issues(&mut self, track: &str, video: &str) {
        self.issues.retain(|issue| match issue {
            SequenceIssue::VideoMissing { track: t, video: v }
            | SequenceIssue::VideoFailed {
                track: t, video: v, ..
            } => !(t == track && v == video),
            _ => true,
        });
    }

    /// Records an issue; `true` the first time (the caller logs it once).
    pub(crate) fn report(&mut self, issue: SequenceIssue) -> bool {
        if self.issues.contains(&issue) {
            return false;
        }
        tracing::warn!(target: "regia_framework", sequence = %self.sequence.name, "{issue}");
        self.issues.insert(issue);
        true
    }
}

/// What a track target name is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// The entity carrying this scene guid ([`EntityGuid`]): survives save, load and level moves.
    Guid(Guid),
    /// This entity, for targets spawned at runtime.
    Entity(Entity),
}

/// Maps track target names to entities for the [`SequencePlayer`] on the same entity.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct SequenceBindings {
    targets: BTreeMap<String, Binding>,
}

impl SequenceBindings {
    /// Binds `target` (a track's `target` name), replacing any binding it had.
    pub fn bind(&mut self, target: impl Into<String>, binding: Binding) {
        self.targets.insert(target.into(), binding);
    }

    /// The same bindings with one more.
    #[must_use]
    pub fn with(mut self, target: impl Into<String>, binding: Binding) -> Self {
        self.bind(target, binding);
        self
    }

    /// Removes a binding; `true` when there was one.
    pub fn unbind(&mut self, target: &str) -> bool {
        self.targets.remove(target).is_some()
    }

    /// The binding of one target name.
    #[must_use]
    pub fn get(&self, target: &str) -> Option<Binding> {
        self.targets.get(target).copied()
    }

    /// Every binding, by target name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Binding)> {
        self.targets
            .iter()
            .map(|(name, binding)| (name.as_str(), *binding))
    }
}

/// Why an applier refused a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    /// The track's channel is the wrong kind for the property.
    #[error("the property takes a {expected} channel")]
    WrongKind {
        /// The channel kind the property takes (`Float`, `Vec3`, `Bool`, or a list of them).
        expected: &'static str,
    },
    /// The target entity lacks the component the property writes.
    #[error("the target has no {0} component")]
    MissingComponent(&'static str),
}

/// Writes one sampled value to a target entity.
pub type PropertyApplier = fn(&mut EntityWorldMut<'_>, ChannelValue) -> Result<(), ApplyError>;

/// The property appliers, by property path. [`SequencerPlugin`] inserts the built-ins; a host adds
/// its own with [`register`](Self::register) (a later registration of the same path wins).
///
/// | Path | Channel | Writes |
/// |---|---|---|
/// | `transform.position` | Vec3 | `Transform::translation` (metres) |
/// | `transform.rotation` | Vec3 | `Transform::rotation` from Euler degrees `[pitch x, yaw y, roll z]`, yaw then pitch then roll |
/// | `transform.scale` | Vec3 or Float | `Transform::scale` (a Float scales uniformly) |
/// | `visible` | Bool | `Visibility::Visible` / `Visibility::Hidden` |
/// | `light.intensity` | Float | `PointLight`/`SpotLight` intensity or `DirectionalLight` illuminance |
/// | `light.color` | Vec3 | the light's linear RGB color |
/// | `camera.fov` | Float | `Camera::fov_y` from degrees |
#[derive(Resource, Debug, Clone)]
pub struct PropertyAppliers {
    appliers: HashMap<String, PropertyApplier>,
}

impl Default for PropertyAppliers {
    fn default() -> Self {
        let mut appliers = Self {
            appliers: HashMap::new(),
        };
        appliers.register("transform.position", apply_position);
        appliers.register("transform.rotation", apply_rotation);
        appliers.register("transform.scale", apply_scale);
        appliers.register("visible", apply_visible);
        appliers.register("light.intensity", apply_light_intensity);
        appliers.register("light.color", apply_light_color);
        appliers.register("camera.fov", apply_camera_fov);
        appliers
    }
}

impl PropertyAppliers {
    /// Registers (or replaces) the applier of one property path.
    pub fn register(&mut self, property: impl Into<String>, applier: PropertyApplier) {
        self.appliers.insert(property.into(), applier);
    }

    /// The applier of one property path.
    #[must_use]
    pub fn get(&self, property: &str) -> Option<PropertyApplier> {
        self.appliers.get(property).copied()
    }

    /// Every registered path, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<&str> {
        let mut paths: Vec<&str> = self.appliers.keys().map(String::as_str).collect();
        paths.sort_unstable();
        paths
    }
}

fn transform_of<'a>(entity: &'a mut EntityWorldMut<'_>) -> Result<Mut<'a, Transform>, ApplyError> {
    entity
        .get_mut::<Transform>()
        .ok_or(ApplyError::MissingComponent("Transform"))
}

fn apply_position(entity: &mut EntityWorldMut<'_>, value: ChannelValue) -> Result<(), ApplyError> {
    let ChannelValue::Vec3(v) = value else {
        return Err(ApplyError::WrongKind { expected: "Vec3" });
    };
    // A root's track is authored in world meters; a child's is relative to its parent (spec 78 § 12.2).
    let offset = if entity.contains::<ChildOf>() {
        None
    } else {
        entity.world().get_resource::<OriginOffset>().map(|o| o.0)
    };
    let local = offset.map_or(Vec3::from_array(v), |o| {
        Vec3::from_array(v) - Vec3::from_array(o.map(|axis| axis as f32))
    });
    transform_of(entity)?.translation = local;
    Ok(())
}

fn apply_rotation(entity: &mut EntityWorldMut<'_>, value: ChannelValue) -> Result<(), ApplyError> {
    let ChannelValue::Vec3([pitch, yaw, roll]) = value else {
        return Err(ApplyError::WrongKind { expected: "Vec3" });
    };
    // The pinned euler construction (spec 35 § 12.13 class): the same rotation bits on every
    // platform, where `Quat::from_euler`'s platform `sin`/`cos` would differ in the last digit.
    transform_of(entity)?.rotation =
        det_quat_from_yaw_pitch_roll(yaw.to_radians(), pitch.to_radians(), roll.to_radians());
    Ok(())
}

fn apply_scale(entity: &mut EntityWorldMut<'_>, value: ChannelValue) -> Result<(), ApplyError> {
    let scale = match value {
        ChannelValue::Vec3(v) => Vec3::from_array(v),
        ChannelValue::Float(s) => Vec3::splat(s),
        ChannelValue::Bool(_) => {
            return Err(ApplyError::WrongKind {
                expected: "Vec3 or Float",
            });
        }
    };
    transform_of(entity)?.scale = scale;
    Ok(())
}

fn apply_visible(entity: &mut EntityWorldMut<'_>, value: ChannelValue) -> Result<(), ApplyError> {
    let ChannelValue::Bool(visible) = value else {
        return Err(ApplyError::WrongKind { expected: "Bool" });
    };
    let wanted = if visible {
        Visibility::Visible
    } else {
        Visibility::Hidden
    };
    match entity.get_mut::<Visibility>() {
        Some(mut current) => {
            if *current != wanted {
                *current = wanted;
            }
        }
        None => {
            entity.insert(wanted);
        }
    }
    Ok(())
}

fn apply_light_intensity(entity: &mut EntityWorldMut<'_>, value: ChannelValue) -> Result<(), ApplyError> {
    let ChannelValue::Float(intensity) = value else {
        return Err(ApplyError::WrongKind { expected: "Float" });
    };
    if let Some(mut light) = entity.get_mut::<PointLight>() {
        light.intensity = intensity;
    } else if let Some(mut light) = entity.get_mut::<SpotLight>() {
        light.intensity = intensity;
    } else if let Some(mut light) = entity.get_mut::<DirectionalLight>() {
        light.illuminance = intensity;
    } else {
        return Err(ApplyError::MissingComponent("light"));
    }
    Ok(())
}

fn apply_light_color(entity: &mut EntityWorldMut<'_>, value: ChannelValue) -> Result<(), ApplyError> {
    let ChannelValue::Vec3(color) = value else {
        return Err(ApplyError::WrongKind { expected: "Vec3" });
    };
    if let Some(mut light) = entity.get_mut::<PointLight>() {
        light.color = color;
    } else if let Some(mut light) = entity.get_mut::<SpotLight>() {
        light.color = color;
    } else if let Some(mut light) = entity.get_mut::<DirectionalLight>() {
        light.color = color;
    } else {
        return Err(ApplyError::MissingComponent("light"));
    }
    Ok(())
}

fn apply_camera_fov(entity: &mut EntityWorldMut<'_>, value: ChannelValue) -> Result<(), ApplyError> {
    let ChannelValue::Float(degrees) = value else {
        return Err(ApplyError::WrongKind { expected: "Float" });
    };
    let mut camera = entity
        .get_mut::<Camera>()
        .ok_or(ApplyError::MissingComponent("Camera"))?;
    camera.fov_y = degrees.to_radians();
    Ok(())
}

/// A marker crossed by a [`SequencePlayer`] step (gameplay or external clock).
///
/// The messages of one frame are ordered by player (entity order), then by firing order: time,
/// then name; across a loop wrap, the markers before the wrap come first.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct MarkerReached {
    /// The player entity.
    pub player: Entity,
    /// The marker's name.
    pub name: String,
    /// The marker's time on the sequence clock, seconds.
    pub time: f64,
    /// The marker's payload (a cue, a flow event, a note; the host interprets it).
    pub payload: String,
    /// How many times the clock had wrapped when it fired.
    pub cycle: u32,
    /// `true` when a skip jumped over the marker ([`SequencePlayer::skip_to`], ADR-2948).
    pub skipped: bool,
}

/// An event key crossed by a [`SequencePlayer`] step (`.seq.ron` v6, ADR-8105): what game code
/// reads to react to an event track, as it reads [`MarkerReached`] for markers.
///
/// The messages of one frame are ordered by player (entity order), then by firing order: time,
/// then track order, then file order; across a loop wrap, the keys before the wrap come first. A
/// seek, a scrub and reverse play send none; a skip sends the keys it jumps over with `skipped`.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct SequenceEventFired {
    /// The player entity.
    pub player: Entity,
    /// The event track's name.
    pub track: String,
    /// The track's binding (`target`; empty for a sequence-wide event).
    pub target: String,
    /// The event name.
    pub event: String,
    /// The key's payload (the host interprets it).
    pub payload: String,
    /// The key's time on the sequence clock, seconds.
    pub time: f64,
    /// How many times the clock had wrapped when it fired.
    pub cycle: u32,
    /// `true` when a skip jumped over the key ([`SequencePlayer::skip_to`]).
    pub skipped: bool,
}

/// A [`SequencePlayer`] reached its end under [`EndPolicy::Hold`] or [`EndPolicy::Once`].
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceFinished {
    /// The player entity.
    pub player: Entity,
    /// The policy it ended under.
    pub end: EndPolicy,
}

/// Counts player starts so the most recent start wins a contested camera.
#[derive(Resource, Debug, Default)]
pub struct SequenceStarts {
    next: u64,
}

/// The order of the sequencer's work in `PostUpdate`, all before the camera rigs
/// ([`CameraSet::Update`]): advance the clocks and send markers, then apply the track values. The
/// camera takeover runs later, in [`CameraSet::Cinematic`].
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SequencerSet {
    /// Clocks advance, markers are sent, bindings resolve.
    Advance,
    /// Track values are written through the [`PropertyAppliers`].
    Apply,
}

/// The entity of every guid, rebuilt only when guids are added or removed.
#[derive(Default)]
struct GuidIndex {
    built: bool,
    map: HashMap<Guid, Entity>,
}

/// What the advance system needs to resolve bindings.
#[derive(SystemParam)]
struct BindingLookup<'w, 's> {
    guids: Query<'w, 's, (Entity, &'static EntityGuid)>,
    added: Query<'w, 's, (), Added<EntityGuid>>,
    removed: RemovedComponents<'w, 's, EntityGuid>,
    live: Query<'w, 's, ()>,
    index: Local<'s, GuidIndex>,
}

impl BindingLookup<'_, '_> {
    fn refresh(&mut self) {
        let removed = self.removed.read().count() > 0;
        if self.index.built && !removed && self.added.is_empty() {
            return;
        }
        let map = self.guids.iter().map(|(entity, guid)| (guid.0, entity)).collect();
        self.index.map = map;
        self.index.built = true;
    }

    fn resolve(&self, binding: Binding) -> Option<Entity> {
        match binding {
            Binding::Entity(entity) => self.live.contains(entity).then_some(entity),
            Binding::Guid(guid) => self
                .index
                .map
                .get(&guid)
                .copied()
                .filter(|&entity| self.live.contains(entity)),
        }
    }
}

/// The messages `advance_players` writes: markers reached, sequences finished and timeline events.
#[derive(SystemParam)]
struct SequenceMessages<'w> {
    markers: MessageWriter<'w, MarkerReached>,
    finished: MessageWriter<'w, SequenceFinished>,
    events: MessageWriter<'w, SequenceEventFired>,
}

/// Advances every player, sends its markers and resolves its bindings.
#[tracing::instrument(skip_all, target = "regia_framework", name = "sequencer_advance")]
fn advance_players(
    time: Res<Time>,
    mut starts: ResMut<SequenceStarts>,
    mut players: Query<(Entity, &mut SequencePlayer, Option<&SequenceBindings>)>,
    mut lookup: BindingLookup,
    mut out: SequenceMessages,
    library: Option<Res<SequenceLibrary>>,
) {
    if players.is_empty() {
        return;
    }
    lookup.refresh();
    let mut order: Vec<Entity> = players.iter().map(|(entity, _, _)| entity).collect();
    order.sort_unstable_by_key(|entity| entity.to_bits());
    for entity in order {
        let Ok((entity, mut player, bindings)) = players.get_mut(entity) else {
            continue;
        };
        if let Some(library) = library.as_deref()
            && player.has_sub_sequences()
            && player.library_revision != Some(library.revision())
        {
            player.set_library(library);
        }
        if std::mem::take(&mut player.start_requested) {
            player.started = Some(starts.next);
            starts.next += 1;
        }
        // An external write this frame already moved the clock: the transition is judged from
        // before it, and its overshoot is kept.
        let external = player.external_showed.take();
        let showed_shot = external.unwrap_or_else(|| player.active_shot().is_some());
        if external.is_none() {
            player.finish_overshoot_ns = 0;
        }
        player.step(time.delta());
        let shows_shot = player.active_shot().is_some();
        // The shots came into view during this step (a start delay ending, a loop wrapping back
        // into a shorter shot list): they have been visible for the clock's time since zero.
        let step_ns = u64::try_from(time.delta().as_nanos()).unwrap_or(u64::MAX);
        player.acquire_ns = (!showed_shot && shows_shot && player.clock == ClockSource::Gameplay)
            .then(|| player.clock_ns.min(step_ns));
        player.release_ns = if showed_shot && !shows_shot && player.takeover.enabled {
            if player.status == PlaybackStatus::Finished {
                // The shots ended at their own end, which may be before the player's end.
                Some(
                    player
                        .finish_overshoot_ns
                        .saturating_add(player.duration_ns - player.shots_duration_ns),
                )
            } else if player.clock_ns > player.shots_duration_ns {
                Some(player.clock_ns - player.shots_duration_ns)
            } else {
                None
            }
        } else {
            None
        };
        for fired in player.take_fired() {
            out.markers.write(MarkerReached {
                player: entity,
                name: fired.name,
                time: fired.time,
                payload: fired.payload,
                cycle: fired.cycle,
                skipped: fired.skipped,
            });
        }
        for fired in player.take_fired_events() {
            out.events.write(SequenceEventFired {
                player: entity,
                track: fired.track,
                target: fired.target,
                event: fired.event,
                payload: fired.payload,
                time: fired.time,
                cycle: fired.cycle,
                skipped: fired.skipped,
            });
        }
        if std::mem::take(&mut player.just_finished) && !player.finish_reported {
            player.finish_reported = true;
            out.finished.write(SequenceFinished {
                player: entity,
                end: player.end,
            });
            // The end frame is applied once even under `Once`.
            player.apply_now = true;
        }

        // Resolve each track's target (no allocation unless something is reported).
        let tracks = player.sequence.tracks.len();
        let mut resolved = std::mem::take(&mut player.resolved);
        resolved.clear();
        resolved.reserve(tracks);
        let mut unbound = Vec::new();
        for track in &player.sequence.tracks {
            // An event track's `target` is only a name the event carries: it binds nothing.
            let binds = !matches!(
                track.keys,
                Channel::Audio(_) | Channel::Sub(_) | Channel::Video(_) | Channel::Event(_)
            );
            let entity = bindings
                .filter(|_| binds)
                .and_then(|b| b.get(&track.target))
                .and_then(|binding| lookup.resolve(binding));
            if entity.is_none() && track.enabled && binds {
                unbound.push(track.target.clone());
            }
            resolved.push(entity);
        }
        player.resolved = resolved;
        // Every name of the tree (sub-sequence targets after their renames). Names already in
        // the map keep their `String`; only a new name allocates.
        let mut entities = std::mem::take(&mut player.target_entities);
        entities.retain(|name, _| player.targets.iter().any(|target| target == name));
        for name in &player.targets {
            let entity = bindings
                .and_then(|b| b.get(name))
                .and_then(|binding| lookup.resolve(binding));
            match entity {
                Some(entity) => {
                    if let Some(slot) = entities.get_mut(name.as_str()) {
                        *slot = entity;
                    } else {
                        #[cfg(test)]
                        hot_counts::add_target();
                        entities.insert(name.clone(), entity);
                    }
                }
                None => {
                    entities.remove(name.as_str());
                    if !unbound.contains(name) {
                        unbound.push(name.clone());
                    }
                }
            }
        }
        player.target_entities = entities;
        // A target that binds now is no longer reported.
        if !player.issues.is_empty() {
            let SequencePlayer {
                issues,
                sequence,
                resolved,
                target_entities,
                ..
            } = &mut *player;
            issues.retain(|issue| match issue {
                SequenceIssue::UnboundTarget { target } => {
                    !(target_entities.contains_key(target)
                        || sequence
                            .tracks
                            .iter()
                            .zip(resolved.iter())
                            .any(|(track, entity)| &track.target == target && entity.is_some()))
                }
                _ => true,
            });
        }
        if player.wants_apply() {
            for target in unbound {
                // A spawnable's name binds to what the player spawns, not to a hand-made
                // binding: out of its range (or after a failed load, reported once as
                // `seq_spawn_failed`) its tracks are skipped silently.
                if player.spawn_names.binary_search(&target).is_ok() {
                    continue;
                }
                player.report(SequenceIssue::UnboundTarget { target });
            }
            player.warm_frame();
        }
    }
}

/// Writes the values of every active player's tracks, in player then track order, then blends
/// its clip tracks onto their rigs.
fn apply_tracks(world: &mut World, players: &mut QueryState<(Entity, &SequencePlayer)>) {
    let _span = tracing::info_span!(target: "regia_framework", "sequencer_apply").entered();
    struct Job {
        player: Entity,
        track: TrackLabel,
        target: Entity,
        applier: PropertyApplier,
        value: ChannelValue,
    }
    struct ClipJob {
        player: Entity,
        track: TrackLabel,
        target: Entity,
        blends: Vec<(Arc<AnimationClip>, f32, f32)>,
    }
    let mut jobs: Vec<Job> = Vec::new();
    let mut clip_jobs: Vec<ClipJob> = Vec::new();
    let mut posed: Vec<Entity> = Vec::new();
    let mut unknown: Vec<(Entity, SequenceIssue)> = Vec::new();
    let mut reflect_jobs: Vec<(Entity, String, Entity, String, ChannelValue)> = Vec::new();
    let mut done: Vec<Entity> = Vec::new();
    // `restore_on_stop` (ADR-2948): put back what players that let go had written, then note the
    // properties this frame writes for the first time.
    restore_released(world, players);
    let mut first_writes: Vec<(Entity, Entity, String)> = Vec::new();
    {
        let Some(appliers) = world.get_resource::<PropertyAppliers>() else {
            return;
        };
        let restore = world.get_resource::<SequenceRestore>();
        let clips = world.get_resource::<SequenceClips>();
        let mut order: Vec<(Entity, &SequencePlayer)> = players.iter(world).collect();
        order.sort_unstable_by_key(|(entity, _)| entity.to_bits());
        for (entity, player) in order {
            if !player.wants_apply() {
                continue;
            }
            done.push(entity);
            let frame = player.frame();
            for value in &frame.values {
                let target = if value.depth == 0 {
                    player.resolved.get(value.track).copied().flatten()
                } else {
                    player.target_entities.get(value.target).copied()
                };
                let Some(target) = target else { continue };
                if player.restore_on_stop
                    // A spawned actor is the player's own: nothing to put back on it.
                    && world.get::<super::spawnables::SpawnedBySequence>(target).is_none()
                    && restore.is_some_and(|r| !r.has(entity, target, value.property))
                    && !first_writes
                        .iter()
                        .any(|(p, t, q)| *p == entity && *t == target && q == value.property)
                {
                    first_writes.push((entity, target, value.property.to_string()));
                }
                match appliers.get(value.property) {
                    Some(applier) => jobs.push(Job {
                        player: entity,
                        track: track_label(value.depth, value.track, value.name),
                        target,
                        applier,
                        value: value.value,
                    }),
                    // A reflected field with no applier of its own (what the take recorder
                    // writes for a `reflect:` channel, ADR-2637).
                    None if value.property.starts_with(super::recorder::REFLECT_PREFIX) => {
                        reflect_jobs.push((
                            entity,
                            value.name.to_string(),
                            target,
                            value.property.to_string(),
                            value.value,
                        ));
                    }
                    None => unknown.push((
                        entity,
                        SequenceIssue::UnknownProperty {
                            track: value.name.to_string(),
                            property: value.property.to_string(),
                        },
                    )),
                }
            }
            for sample in &frame.clips {
                let Some(target) = player.target_entities.get(sample.target).copied() else {
                    continue;
                };
                let blends = clip_blends(sample, clips, |clip| {
                    unknown.push((
                        entity,
                        SequenceIssue::UnknownClip {
                            track: sample.name.to_string(),
                            clip: clip.to_string(),
                        },
                    ));
                });
                if !blends.is_empty() {
                    if !posed.contains(&target) {
                        posed.push(target);
                    }
                    clip_jobs.push(ClipJob {
                        player: entity,
                        track: track_label(sample.depth, sample.track, sample.name),
                        target,
                        blends,
                    });
                }
            }
        }
    }
    capture_first_writes(world, first_writes);
    let mut failures: Vec<(Entity, TrackLabel, ApplyError)> = Vec::new();
    for job in jobs {
        let Ok(mut target) = world.get_entity_mut(job.target) else {
            continue;
        };
        if let Err(error) = (job.applier)(&mut target, job.value) {
            failures.push((job.player, job.track, error));
        }
    }
    for (player, track, target, property, value) in reflect_jobs {
        if let Err(reason) = super::recorder::apply_reflected(world, target, &property, value) {
            unknown.push((player, SequenceIssue::ApplyFailed { track, reason }));
        }
    }
    // Every rig starts the frame from its own pose: the blends of this frame go over that, not
    // over the blends of the last frame, and a rig no track poses any more gets it back.
    world.resource_scope(|world, mut rigs: Mut<ClipRigs>| {
        rigs.release_all_but(world, &posed);
        for &rig in &posed {
            rigs.begin(world, rig);
        }
        for job in clip_jobs {
            if let Err(error) = blend_clips_into_with(world, job.target, &job.blends, &mut rigs.scratch) {
                failures.push((job.player, job.track, error));
            }
        }
        for &rig in &posed {
            rigs.end(world, rig);
        }
    });
    for entity in done {
        if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
            player.apply_now = false;
        }
    }
    for (entity, issue) in unknown {
        if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
            player.report(issue);
        }
    }
    for (entity, track, error) in failures {
        if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
            let track = match track {
                TrackLabel::Root(index) => player
                    .sequence
                    .tracks
                    .get(index)
                    .map(|track| track.name.clone())
                    .unwrap_or_default(),
                TrackLabel::Sub(name) => name,
            };
            player.report(SequenceIssue::ApplyFailed {
                track,
                reason: error.to_string(),
            });
        }
    }
}

/// Where a job's track name lives. The success path does not copy it.
enum TrackLabel {
    /// Index into the player's own sequence.
    Root(usize),
    /// A sub-sequence track, named only because the root sequence does not hold it.
    Sub(String),
}

/// A root track is named from the sequence when a write fails. A deeper track keeps its name,
/// because that string is not on the player's sequence.
fn track_label(depth: usize, index: usize, name: &str) -> TrackLabel {
    if depth == 0 {
        TrackLabel::Root(index)
    } else {
        #[cfg(test)]
        hot_counts::add_track();
        TrackLabel::Sub(name.to_string())
    }
}

/// What `restore_on_stop` players overwrote (ADR-2948): per player, each `(target, property)` it
/// wrote with the value it had before the first write, in first-write order. Kept in a resource,
/// not on the player, so a despawned player's values are put back too.
#[derive(Resource, Debug, Default)]
pub struct SequenceRestore {
    saved: BTreeMap<Entity, Vec<(Entity, String, Option<ChannelValue>)>>,
    restored: u64,
}

impl SequenceRestore {
    fn has(&self, player: Entity, target: Entity, property: &str) -> bool {
        self.saved
            .get(&player)
            .is_some_and(|list| list.iter().any(|(t, p, _)| *t == target && p == property))
    }

    /// The saved `(target, property, prior value)` entries of one player, in first-write order;
    /// `None` for a property that has no reader (noted once, not restored).
    #[must_use]
    pub fn saved(&self, player: Entity) -> &[(Entity, String, Option<ChannelValue>)] {
        self.saved.get(&player).map_or(&[], Vec::as_slice)
    }

    /// Values written back since the app started.
    #[must_use]
    pub fn restored(&self) -> u64 {
        self.restored
    }
}

/// Reads, through the take recorder's `PropertyReaders`, the values about to be overwritten for
/// the first time. A property with no reader (or a target that refuses) cannot be restored and is
/// noted as having no value, so it is not read again.
fn capture_first_writes(world: &mut World, first_writes: Vec<(Entity, Entity, String)>) {
    if first_writes.is_empty() {
        return;
    }
    let mut captured = Vec::with_capacity(first_writes.len());
    {
        let readers = world.get_resource::<super::recorder::PropertyReaders>();
        for (player, target, property) in first_writes {
            let value = if property.starts_with(super::recorder::REFLECT_PREFIX) {
                super::recorder::read_reflected(world, target, &property).ok()
            } else {
                readers
                    .and_then(|r| r.get(&property))
                    .and_then(|read| read(world, target).ok())
            };
            captured.push((player, target, property, value));
        }
    }
    let Some(mut restore) = world.get_resource_mut::<SequenceRestore>() else {
        return;
    };
    for (player, target, property, value) in captured {
        if value.is_none() {
            tracing::debug!(target: "regia_framework", %property, "restore_on_stop: no reader, not restored");
        }
        // Noted either way, so a property without a reader is not read again every frame.
        restore
            .saved
            .entry(player)
            .or_default()
            .push((target, property, value));
    }
}

/// Writes back the saved values of every player that no longer applies (stopped, let go at the
/// end, `restore_on_stop` turned off, or despawned), last write first.
fn restore_released(world: &mut World, players: &mut QueryState<(Entity, &SequencePlayer)>) {
    let released: Vec<Entity> = {
        let Some(restore) = world.get_resource::<SequenceRestore>() else {
            return;
        };
        if restore.saved.is_empty() {
            return;
        }
        restore
            .saved
            .keys()
            .copied()
            .filter(|&player| {
                players
                    .get(world, player)
                    .ok()
                    .is_none_or(|(_, p)| !p.wants_apply() || !p.restore_on_stop)
            })
            .collect()
    };
    for player in released {
        let entries = world
            .get_resource_mut::<SequenceRestore>()
            .and_then(|mut r| r.saved.remove(&player))
            .unwrap_or_default();
        let mut written = 0u64;
        for (target, property, value) in entries.into_iter().rev() {
            let Some(value) = value else { continue };
            let applier = world
                .get_resource::<PropertyAppliers>()
                .and_then(|a| a.get(&property));
            let result = match applier {
                Some(applier) => match world.get_entity_mut(target) {
                    Ok(mut entity) => applier(&mut entity, value).map_err(|e| e.to_string()),
                    Err(_) => continue,
                },
                None if property.starts_with(super::recorder::REFLECT_PREFIX) => {
                    super::recorder::apply_reflected(world, target, &property, value)
                }
                None => continue,
            };
            match result {
                Ok(()) => written += 1,
                Err(reason) => {
                    tracing::warn!(target: "regia_framework", %property, "restore_on_stop: {reason}");
                }
            }
        }
        if let Some(mut restore) = world.get_resource_mut::<SequenceRestore>() {
            restore.restored = restore.restored.saturating_add(written);
        }
    }
}

/// A joint's local translation, rotation and scale.
type JointPose = (Vec3, Quat, Vec3);

/// The joints of `rig` with their poses (a joint without a `Transform` is `None`).
fn joint_poses(world: &World, rig: Entity) -> Vec<(Entity, Option<JointPose>)> {
    let Some(binding) = world.get::<SkinnedMesh>(rig) else {
        return Vec::new();
    };
    binding
        .joints
        .iter()
        .map(|&joint| {
            let pose = world
                .get::<Transform>(joint)
                .map(|t| (t.translation, t.rotation, t.scale));
            (joint, pose)
        })
        .collect()
}

/// Writes `pose` to `joint`.
fn set_joint(world: &mut World, joint: Entity, pose: JointPose) {
    if let Some(mut transform) = world.get_mut::<Transform>(joint) {
        (transform.translation, transform.rotation, transform.scale) = pose;
    }
}

/// One rig clip tracks are posing: each joint's own pose and what the blends left on it.
#[derive(Debug, Default)]
struct RigPose {
    own: Vec<Option<JointPose>>,
    written: Vec<Option<JointPose>>,
}

/// The rigs without an `Animator` that clip tracks are posing. Nothing rewrites such a rig each
/// frame, so its own pose is kept here: blending over the joints would compound frame by frame
/// and depend on the frame rate, and a blend out would never get back. A joint that changed
/// since the last blend was moved by something else, and that is its own pose from then on.
#[derive(Resource, Debug, Default)]
pub(crate) struct ClipRigs {
    rigs: BTreeMap<Entity, RigPose>,
    /// Pose buffers reused by every clip blend this player applies.
    scratch: PoseScratch,
}

/// Scratch poses for `blend_clips_into_with`. Resized when the joint count changes.
#[derive(Debug, Default)]
struct PoseScratch {
    current: regia_animation::pose::Pose,
    sample: regia_animation::pose::Pose,
    mixed: regia_animation::pose::Pose,
    result: regia_animation::pose::Pose,
    animated: Vec<bool>,
    joints: Vec<Entity>,
}

impl ClipRigs {
    /// Puts `rig` in its own pose before this frame's blends.
    fn begin(&mut self, world: &mut World, rig: Entity) {
        if world.get::<regia_animation::Animator>(rig).is_some() {
            return;
        }
        let now = joint_poses(world, rig);
        let entry = self.rigs.entry(rig).or_default();
        entry.own.resize(now.len(), None);
        entry.written.resize(now.len(), None);
        for (i, &(joint, pose)) in now.iter().enumerate() {
            if pose != entry.written[i] {
                entry.own[i] = pose;
            }
            if let Some(own) = entry.own[i].filter(|own| pose != Some(*own)) {
                set_joint(world, joint, own);
            }
        }
    }

    /// Records what this frame's blends left on `rig`.
    fn end(&mut self, world: &World, rig: Entity) {
        if let Some(entry) = self.rigs.get_mut(&rig) {
            entry.written = joint_poses(world, rig).iter().map(|(_, pose)| *pose).collect();
        }
    }

    /// Gives every rig not in `posed` its own pose back and forgets it. A joint something else
    /// moved since the last blend stays where it is.
    fn release_all_but(&mut self, world: &mut World, posed: &[Entity]) {
        let done: Vec<Entity> = self
            .rigs
            .keys()
            .copied()
            .filter(|rig| !posed.contains(rig))
            .collect();
        for rig in done {
            let Some(entry) = self.rigs.remove(&rig) else {
                continue;
            };
            for (i, (joint, pose)) in joint_poses(world, rig).into_iter().enumerate() {
                let own = entry.own.get(i).copied().flatten();
                if let Some(own) = own.filter(|_| entry.written.get(i).copied().flatten() == pose) {
                    set_joint(world, joint, own);
                }
            }
        }
    }
}

/// The animation clips clip tracks name, by name. A resource the host (or `sequence.player
/// { action: "clip" }`) fills; a clip track's section names a clip here.
#[derive(Resource, Debug, Clone, Default)]
pub struct SequenceClips {
    clips: BTreeMap<String, Arc<AnimationClip>>,
}

impl SequenceClips {
    /// Adds or replaces a clip under `name`.
    pub fn insert(&mut self, name: impl Into<String>, clip: AnimationClip) {
        self.clips.insert(name.into(), Arc::new(clip));
    }

    /// Removes a clip; `true` when it was there.
    pub fn remove(&mut self, name: &str) -> bool {
        self.clips.remove(name).is_some()
    }

    /// The clip under `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<AnimationClip>> {
        self.clips.get(name)
    }

    /// The names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.clips.keys().map(String::as_str)
    }

    /// How many clips it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.clips.len()
    }

    /// `true` when it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }
}

/// The clip time a section plays: wrapped over the clip's length when looping, else clamped into
/// it.
#[must_use]
pub fn wrap_clip_time(time: f64, duration: f32, looping: bool) -> f32 {
    let duration = f64::from(duration.max(0.0));
    if duration <= 0.0 {
        return 0.0;
    }
    if looping {
        time.rem_euclid(duration) as f32
    } else {
        time.clamp(0.0, duration) as f32
    }
}

/// The clips of a sample with their wrapped times and weights; `missing` hears each name the
/// library lacks.
fn clip_blends(
    sample: &ClipSample<'_>,
    clips: Option<&SequenceClips>,
    mut missing: impl FnMut(&str),
) -> Vec<(Arc<AnimationClip>, f32, f32)> {
    let mut out = Vec::with_capacity(sample.blends.len());
    for blend in &sample.blends {
        match clips.and_then(|c| c.get(blend.clip)) {
            Some(clip) => {
                let time = wrap_clip_time(blend.clip_time, clip.duration, blend.looping);
                out.push((Arc::clone(clip), time, blend.weight));
            }
            None => missing(blend.clip),
        }
    }
    out
}

/// Blends `blends` (clip, clip time, weight) onto the joints of the rig `rig` (an entity with a
/// [`SkinnedMesh`]) and returns how many joints were written.
///
/// Each clip is sampled over the joints' current pose (a joint a clip does not animate keeps
/// its pose in that clip), the clips are mixed by weight (normalized running blend), and when
/// the weights sum below one the result is mixed over the current pose by that sum, so a blend
/// in starts from whatever pose the rig had. Only joints some clip animates are written.
///
/// On a rig with an `Animator` the current pose is the animator's evaluated pose, and the blend
/// goes back into it: the player's apply step runs before [`PostUpdateSet::AnimationApply`], so
/// ragdoll blending, rig drivers, spring bones, gaze, instance proportions, the joints, the skin
/// palette and sockets all follow the blended pose. A rig the animator has not evaluated yet
/// writes nothing.
pub fn blend_clips_into(
    world: &mut World,
    rig: Entity,
    blends: &[(Arc<AnimationClip>, f32, f32)],
) -> Result<usize, ApplyError> {
    let mut scratch = PoseScratch::default();
    blend_clips_into_with(world, rig, blends, &mut scratch)
}

/// [`blend_clips_into`] with pose buffers the caller keeps across frames.
fn blend_clips_into_with(
    world: &mut World,
    rig: Entity,
    blends: &[(Arc<AnimationClip>, f32, f32)],
    scratch: &mut PoseScratch,
) -> Result<usize, ApplyError> {
    let skeleton = {
        let Some(binding) = world.get::<SkinnedMesh>(rig) else {
            return Err(ApplyError::MissingComponent("SkinnedMesh"));
        };
        if scratch.joints.as_slice() != binding.joints.as_slice() {
            scratch.joints.clear();
            scratch.joints.extend_from_slice(&binding.joints);
        }
        Arc::clone(&binding.skeleton)
    };
    let animator = world.get::<regia_animation::Animator>(rig).is_some();
    let n = if animator {
        let Some(animator) = world.get::<regia_animation::Animator>(rig) else {
            return Ok(0);
        };
        let pose = animator.evaluated_pose();
        if pose.len() != skeleton.len() {
            return Ok(0);
        }
        ensure_pose(&mut scratch.current, pose.len());
        copy_pose(&mut scratch.current, pose);
        pose.len()
    } else {
        let n = skeleton.len().min(scratch.joints.len());
        ensure_pose(&mut scratch.current, n);
        for i in 0..n {
            let bind = &skeleton.bind_pose[i];
            scratch.current.translations[i] = bind.translation;
            scratch.current.rotations[i] = bind.rotation;
            scratch.current.scales[i] = bind.scale;
        }
        for i in 0..n {
            let entity = scratch.joints[i];
            if let Some(transform) = world.get::<Transform>(entity) {
                scratch.current.translations[i] = transform.translation;
                scratch.current.rotations[i] = transform.rotation;
                scratch.current.scales[i] = transform.scale;
            }
        }
        n
    };
    if scratch.animated.len() != n {
        #[cfg(test)]
        hot_counts::add_pose();
        scratch.animated.resize(n, false);
    }
    scratch.animated.fill(false);
    ensure_pose(&mut scratch.sample, n);
    ensure_pose(&mut scratch.mixed, n);
    ensure_pose(&mut scratch.result, n);
    let mut total = 0.0f32;
    let mut have_mixed = false;
    for (clip, time, weight) in blends {
        if *weight <= 0.0 {
            continue;
        }
        copy_pose(&mut scratch.sample, &scratch.current);
        clip.sample_pose(*time, &mut scratch.sample);
        for track in &clip.tracks {
            if let Some(flag) = scratch.animated.get_mut(track.joint as usize) {
                *flag = true;
            }
        }
        total += weight;
        if !have_mixed {
            copy_pose(&mut scratch.mixed, &scratch.sample);
            have_mixed = true;
        } else {
            let weight = weight / total;
            scratch.mixed.blend_from(&scratch.sample, weight);
        }
    }
    if !have_mixed {
        return Ok(0);
    }
    copy_pose(&mut scratch.result, &scratch.current);
    let mix = total.min(1.0);
    scratch.result.blend_from(&scratch.mixed, mix);
    if animator {
        // Joints no clip animates keep the animator's values exactly, not a blend's rounding.
        {
            let PoseScratch {
                current,
                result,
                animated,
                ..
            } = &mut *scratch;
            for (i, animated_joint) in animated.iter().enumerate().take(n) {
                if *animated_joint {
                    continue;
                }
                result.translations[i] = current.translations[i];
                result.rotations[i] = current.rotations[i];
                result.scales[i] = current.scales[i];
            }
        }
        let written = scratch.animated.iter().filter(|animated| **animated).count();
        if let Some(mut animator) = world.get_mut::<regia_animation::Animator>(rig) {
            let dest = animator.evaluated_pose_mut();
            if dest.len() == scratch.result.len() {
                copy_pose(dest, &scratch.result);
            }
        }
        return Ok(written);
    }
    let mut written = 0;
    for i in 0..n {
        if !scratch.animated[i] {
            continue;
        }
        let entity = scratch.joints[i];
        let translation = scratch.result.translations[i];
        let rotation = scratch.result.rotations[i].normalize();
        let scale = scratch.result.scales[i];
        if let Some(mut transform) = world.get_mut::<Transform>(entity) {
            transform.translation = translation;
            transform.rotation = rotation;
            transform.scale = scale;
            written += 1;
        }
    }
    Ok(written)
}

/// Grows `pose` to `n` joints. A second call with the same `n` does not allocate.
fn ensure_pose(pose: &mut regia_animation::pose::Pose, n: usize) {
    if pose.len() == n {
        return;
    }
    #[cfg(test)]
    hot_counts::add_pose();
    pose.translations.resize(n, Vec3::ZERO);
    pose.rotations.resize(n, Quat::IDENTITY);
    pose.scales.resize(n, Vec3::ONE);
}

/// Copies `src` onto `dst`. Both must already have the same length.
fn copy_pose(dst: &mut regia_animation::pose::Pose, src: &regia_animation::pose::Pose) {
    dst.translations.copy_from_slice(&src.translations);
    dst.rotations.copy_from_slice(&src.rotations);
    dst.scales.copy_from_slice(&src.scales);
}

/// An owned [`super::sample::SequenceFrame`], so a later `&self` read can borrow it.
#[derive(Clone, Debug)]
struct CachedFrame {
    time: f64,
    finished: bool,
    values: Vec<CachedValue>,
    clips: Vec<CachedClip>,
    sounds: Vec<CachedSound>,
    videos: Vec<CachedVideo>,
}

#[derive(Clone, Debug)]
struct CachedValue {
    track: usize,
    name: String,
    target: String,
    property: String,
    value: ChannelValue,
    depth: usize,
}

#[derive(Clone, Debug)]
struct CachedWeight {
    section: usize,
    clip: String,
    clip_time: f64,
    looping: bool,
    weight: f32,
}

#[derive(Clone, Debug)]
struct CachedClip {
    track: usize,
    name: String,
    target: String,
    blends: Vec<CachedWeight>,
    depth: usize,
}

#[derive(Clone, Debug)]
struct CachedSound {
    path: Vec<usize>,
    track: String,
    sound: String,
    position: f64,
    rate: f64,
    volume_db: f32,
    fade: f32,
    remaining: f64,
    bus: Option<String>,
    spatial: Option<super::asset::AudioSpatial>,
    emitter: Option<String>,
    time_scale_mode: super::asset::TimeScaleMode,
    section_offset: f64,
    section_source_secs: f64,
}

#[derive(Clone, Debug)]
struct CachedVideo {
    path: Vec<usize>,
    track: String,
    video: String,
    target: super::asset::VideoTarget,
    binding: Option<String>,
    position: f64,
    rate: f64,
    volume_db: f32,
    looping: bool,
}

impl CachedFrame {
    fn capture(player: &SequencePlayer) -> Self {
        let frame = player.sample_now();
        Self {
            time: frame.time,
            finished: frame.finished,
            values: frame
                .values
                .iter()
                .map(|value| CachedValue {
                    track: value.track,
                    name: value.name.to_owned(),
                    target: value.target.to_owned(),
                    property: value.property.to_owned(),
                    value: value.value,
                    depth: value.depth,
                })
                .collect(),
            clips: frame
                .clips
                .iter()
                .map(|clip| CachedClip {
                    track: clip.track,
                    name: clip.name.to_owned(),
                    target: clip.target.to_owned(),
                    blends: clip
                        .blends
                        .iter()
                        .map(|blend| CachedWeight {
                            section: blend.section,
                            clip: blend.clip.to_owned(),
                            clip_time: blend.clip_time,
                            looping: blend.looping,
                            weight: blend.weight,
                        })
                        .collect(),
                    depth: clip.depth,
                })
                .collect(),
            sounds: frame
                .sounds
                .iter()
                .map(|sound| CachedSound {
                    path: sound.path.clone(),
                    track: sound.track.to_owned(),
                    sound: sound.sound.to_owned(),
                    position: sound.position,
                    rate: sound.rate,
                    volume_db: sound.volume_db,
                    fade: sound.fade,
                    remaining: sound.remaining,
                    bus: sound.bus.map(str::to_owned),
                    spatial: sound.spatial.cloned(),
                    emitter: sound.emitter.map(str::to_owned),
                    time_scale_mode: sound.time_scale_mode,
                    section_offset: sound.section_offset,
                    section_source_secs: sound.section_source_secs,
                })
                .collect(),
            videos: frame
                .videos
                .iter()
                .map(|video| CachedVideo {
                    path: video.path.clone(),
                    track: video.track.to_owned(),
                    video: video.video.to_owned(),
                    target: video.target.clone(),
                    binding: video.binding.map(str::to_owned),
                    position: video.position,
                    rate: video.rate,
                    volume_db: video.volume_db,
                    looping: video.looping,
                })
                .collect(),
        }
    }

    fn view(&self) -> super::sample::SequenceFrame<'_> {
        super::sample::SequenceFrame {
            time: self.time,
            finished: self.finished,
            values: self
                .values
                .iter()
                .map(|value| super::sample::TrackSample {
                    track: value.track,
                    name: value.name.as_str(),
                    target: value.target.as_str(),
                    property: value.property.as_str(),
                    value: value.value,
                    depth: value.depth,
                })
                .collect(),
            clips: self
                .clips
                .iter()
                .map(|clip| super::sample::ClipSample {
                    track: clip.track,
                    name: clip.name.as_str(),
                    target: clip.target.as_str(),
                    blends: clip
                        .blends
                        .iter()
                        .map(|blend| super::sample::ClipWeight {
                            section: blend.section,
                            clip: blend.clip.as_str(),
                            clip_time: blend.clip_time,
                            looping: blend.looping,
                            weight: blend.weight,
                        })
                        .collect(),
                    depth: clip.depth,
                })
                .collect(),
            sounds: self
                .sounds
                .iter()
                .map(|sound| super::sample::SoundSample {
                    path: sound.path.clone(),
                    track: sound.track.as_str(),
                    sound: sound.sound.as_str(),
                    position: sound.position,
                    rate: sound.rate,
                    volume_db: sound.volume_db,
                    fade: sound.fade,
                    remaining: sound.remaining,
                    bus: sound.bus.as_deref(),
                    spatial: sound.spatial.as_ref(),
                    emitter: sound.emitter.as_deref(),
                    time_scale_mode: sound.time_scale_mode,
                    section_offset: sound.section_offset,
                    section_source_secs: sound.section_source_secs,
                })
                .collect(),
            videos: self
                .videos
                .iter()
                .map(|video| super::sample::VideoSample {
                    path: video.path.clone(),
                    track: video.track.as_str(),
                    video: video.video.as_str(),
                    target: &video.target,
                    binding: video.binding.as_deref(),
                    position: video.position,
                    rate: video.rate,
                    volume_db: video.volume_db,
                    looping: video.looping,
                })
                .collect(),
        }
    }
}

/// Allocations on the sequencer hot path. Tests reset these around one frame.
#[cfg(test)]
pub(crate) mod hot_counts {
    use std::cell::Cell;

    thread_local! {
        static POSE: Cell<u64> = const { Cell::new(0) };
        static TRACK: Cell<u64> = const { Cell::new(0) };
        static TARGET: Cell<u64> = const { Cell::new(0) };
        static MARKER: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        POSE.with(|c| c.set(0));
        TRACK.with(|c| c.set(0));
        TARGET.with(|c| c.set(0));
        MARKER.with(|c| c.set(0));
    }

    pub(crate) fn pose() -> u64 {
        POSE.with(|c| c.get())
    }

    pub(crate) fn track() -> u64 {
        TRACK.with(|c| c.get())
    }

    pub(crate) fn target() -> u64 {
        TARGET.with(|c| c.get())
    }

    pub(crate) fn marker() -> u64 {
        MARKER.with(|c| c.get())
    }

    pub(crate) fn add_pose() {
        POSE.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn add_track() {
        TRACK.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn add_target() {
        TARGET.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn add_marker() {
        MARKER.with(|c| c.set(c.get() + 1));
    }
}

/// Applies while a player exists or a rig still waits for its own pose back.
fn apply_wanted(
    players: Query<(), With<SequencePlayer>>,
    rigs: Res<ClipRigs>,
    restore: Option<Res<SequenceRestore>>,
) -> bool {
    !players.is_empty() || !rigs.rigs.is_empty() || restore.is_some_and(|r| !r.saved.is_empty())
}

/// Adds the sequence player: the [`PropertyAppliers`] built-ins, the [`MarkerReached`] and
/// [`SequenceFinished`] messages, the advance and apply systems, the camera takeover, and the shot
/// player it builds on ([`crate::shots::ShotPlugin`]). `AgentPlugin` installs it too, so
/// `sequence.player` works in any agent-enabled app.
pub struct SequencerPlugin;

impl Plugin for SequencerPlugin {
    fn build(&self, app: &mut App) {
        install(app);
    }
}

/// Idempotent form of [`SequencerPlugin`]: does nothing when the player is already installed.
pub fn install(app: &mut App) {
    if app.world().contains_resource::<SequenceStarts>() {
        return;
    }
    crate::shots::player::install(app);
    super::stream_prefetch::install(app);
    app.init_resource::<SequenceStarts>()
        .init_resource::<PropertyAppliers>()
        .init_resource::<SequenceClips>()
        .init_resource::<SequenceLibrary>()
        .init_resource::<ClipRigs>()
        .init_resource::<SequenceRestore>()
        .init_resource::<super::spawnables::SequenceSpawnables>()
        .add_message::<super::spawnables::SpawnableChanged>()
        .add_message::<MarkerReached>()
        .add_message::<SequenceEventFired>()
        .add_message::<SequenceFinished>()
        .configure_sets(
            PostUpdate,
            (SequencerSet::Advance, SequencerSet::Apply)
                .chain()
                .after(PostUpdateSet::LateScripts)
                .before(CameraSet::Update)
                // Before the animators' poses reach their rigs: clip tracks blend into them.
                .before(PostUpdateSet::AnimationApply)
                .before(PostUpdateSet::TransformPropagate),
        )
        // An app with no player pays for neither system (the agent installs this everywhere).
        .add_systems(
            PostUpdate,
            (
                super::spawnables::preroll_spawnables.run_if(any_with_component::<SequencePlayer>),
                advance_players.run_if(any_with_component::<SequencePlayer>),
                super::spawnables::sync_spawnables.run_if(super::spawnables::spawn_wanted),
            )
                .chain()
                .in_set(SequencerSet::Advance),
        )
        .add_systems(
            PostUpdate,
            apply_tracks.in_set(SequencerSet::Apply).run_if(apply_wanted),
        );
    super::takeover::install(app);
    super::cutscene::install(app);
    super::audio::install(app);
    super::subtitles::install(app);
    super::recorder::install(app);
    super::video::install(app);
}

#[cfg(test)]
mod tests {
    use super::super::asset::fixtures::{float_track, marker, sequence};
    use super::super::asset::{BoolKey, Channel, Track, Vec3Key};
    use super::*;

    fn names(fired: &[FiredMarker]) -> Vec<String> {
        fired.iter().map(|m| m.name.clone()).collect()
    }

    fn playing(markers: Vec<super::super::asset::Marker>, duration: f32, end: EndPolicy) -> SequencePlayer {
        let mut player = SequencePlayer::new(sequence(
            vec![float_track("t", "ui", "opacity", duration)],
            markers,
        ));
        player.end = end;
        player.play();
        player
    }

    #[test]
    fn seconds_and_nanos_convert_exactly_and_defensively() {
        assert_eq!(seconds_to_nanos(1.5), 1_500_000_000);
        assert_eq!(seconds_to_nanos(-1.0), 0);
        assert_eq!(seconds_to_nanos(f64::NAN), 0);
        assert_eq!(seconds_to_nanos(f64::INFINITY), 0);
        assert_eq!(seconds_to_nanos(1.0e30), u64::MAX);
        assert_eq!(nanos_to_seconds(250_000_000), 0.25);
    }

    #[test]
    fn a_start_at_zero_fires_the_zero_markers_and_steps_are_half_open() {
        let mut p = playing(
            vec![marker("start", 0.0), marker("one", 1.0), marker("two", 2.0)],
            4.0,
            EndPolicy::Hold,
        );
        p.step(Duration::from_millis(500));
        assert_eq!(
            names(&p.take_fired()),
            ["start"],
            "zero is included on the first step"
        );
        p.step(Duration::from_millis(500));
        assert_eq!(names(&p.take_fired()), ["one"], "the step's end is included");
        p.step(Duration::from_millis(500));
        assert!(p.take_fired().is_empty());
        p.step(Duration::ZERO);
        assert!(p.take_fired().is_empty(), "a zero step fires nothing more");
        p.step(Duration::from_millis(500));
        assert_eq!(names(&p.take_fired()), ["two"]);
        assert_eq!(p.markers_fired(), 3);
    }

    #[test]
    fn simultaneous_markers_fire_by_time_then_name() {
        let mut p = playing(
            vec![
                marker("zulu", 1.0),
                marker("late", 1.5),
                marker("alpha", 1.0),
                marker("mike", 1.0),
            ],
            4.0,
            EndPolicy::Hold,
        );
        p.step(Duration::from_secs(2));
        assert_eq!(names(&p.take_fired()), ["alpha", "mike", "zulu", "late"]);
    }

    #[test]
    fn seeking_never_fires_markers() {
        let mut p = playing(vec![marker("a", 1.0), marker("b", 2.0)], 4.0, EndPolicy::Hold);
        p.seek(3.0);
        assert!(p.take_fired().is_empty());
        assert_eq!(p.time(), 3.0);
        p.step(Duration::from_millis(500));
        assert!(
            p.take_fired().is_empty(),
            "the markers behind the seek stay unfired"
        );
        p.seek(0.5);
        p.step(Duration::from_millis(600));
        assert_eq!(
            names(&p.take_fired()),
            ["a"],
            "a step after a seek fires from the new time"
        );
        p.seek(0.0);
        p.step(Duration::from_millis(100));
        assert!(p.take_fired().is_empty());
    }

    #[test]
    fn hold_stops_on_the_end_and_stays_active() {
        let mut p = playing(vec![marker("end", 2.0)], 2.0, EndPolicy::Hold);
        p.step(Duration::from_secs(5));
        assert_eq!(names(&p.take_fired()), ["end"], "a marker on the end fires");
        assert_eq!(p.time(), 2.0);
        assert_eq!(p.status(), PlaybackStatus::Finished);
        assert!(p.is_active(), "hold keeps its outputs");
        p.step(Duration::from_secs(1));
        assert!(p.take_fired().is_empty(), "nothing fires after the end");
        p.play();
        assert_eq!(p.time(), 0.0, "play from the end restarts");
        assert_eq!(p.status(), PlaybackStatus::Playing);
    }

    #[test]
    fn once_stops_and_lets_go() {
        let mut p = playing(vec![], 2.0, EndPolicy::Once);
        p.step(Duration::from_secs(3));
        assert_eq!(p.status(), PlaybackStatus::Finished);
        assert!(!p.is_active(), "once releases its outputs");
    }

    #[test]
    fn loop_wraps_with_end_markers_before_start_markers() {
        let mut p = playing(
            vec![marker("start", 0.0), marker("mid", 1.0), marker("end", 2.0)],
            2.0,
            EndPolicy::Loop,
        );
        p.step(Duration::from_millis(1500));
        assert_eq!(names(&p.take_fired()), ["start", "mid"]);
        p.step(Duration::from_millis(1000));
        let fired = p.take_fired();
        assert_eq!(
            names(&fired),
            ["end", "start"],
            "end, wrap, then the new pass's start"
        );
        assert_eq!(fired[0].cycle, 0);
        assert_eq!(fired[1].cycle, 1);
        assert_eq!(p.cycle(), 1);
        assert!((p.time() - 0.5).abs() < 1.0e-12);
        assert_eq!(p.status(), PlaybackStatus::Playing);
    }

    #[test]
    fn a_step_across_several_loops_replays_each_pass_up_to_the_cap() {
        let mut p = playing(vec![marker("tick", 0.5)], 1.0, EndPolicy::Loop);
        p.step(Duration::from_millis(3250));
        let fired = p.take_fired();
        assert_eq!(names(&fired), ["tick", "tick", "tick"]);
        assert_eq!(fired.iter().map(|m| m.cycle).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(p.cycle(), 3);
        assert!((p.time() - 0.25).abs() < 1.0e-12);
        let mut long = playing(vec![marker("tick", 0.5)], 1.0, EndPolicy::Loop);
        long.step(Duration::from_secs(100));
        let fired = long.take_fired();
        assert_eq!(
            fired.len() as u64,
            1 + MAX_REPLAYED_CYCLES,
            "the first pass plus the cap"
        );
        assert_eq!(long.skipped_cycles(), 100 - 1 - MAX_REPLAYED_CYCLES);
        assert_eq!(long.cycle(), 100);
    }

    #[test]
    fn the_start_delay_holds_the_clock_then_starts_it() {
        let mut p = SequencePlayer::new(sequence(
            vec![float_track("t", "ui", "opacity", 2.0)],
            vec![marker("start", 0.0)],
        ));
        p.start_delay = 1.0;
        p.play();
        assert_eq!(p.status(), PlaybackStatus::Waiting);
        p.step(Duration::from_millis(600));
        assert_eq!(p.time(), 0.0);
        assert!(p.take_fired().is_empty());
        assert!((p.delay_left() - 0.4).abs() < 1.0e-12);
        p.pause();
        p.step(Duration::from_secs(5));
        assert!((p.delay_left() - 0.4).abs() < 1.0e-12, "a paused delay holds");
        p.play();
        assert_eq!(p.status(), PlaybackStatus::Waiting);
        p.step(Duration::from_millis(500));
        assert_eq!(p.status(), PlaybackStatus::Playing);
        assert_eq!(names(&p.take_fired()), ["start"]);
        assert!(
            (p.time() - 0.1).abs() < 1.0e-12,
            "the rest of the step runs the clock"
        );
    }

    #[test]
    fn an_external_clock_ignores_steps_and_follows_the_written_time() {
        let mut p = SequencePlayer::new(sequence(
            vec![float_track("t", "ui", "opacity", 4.0)],
            vec![marker("a", 1.0), marker("b", 3.0)],
        ));
        p.clock = ClockSource::External;
        p.start_delay = 2.0;
        p.play();
        assert_eq!(
            p.status(),
            PlaybackStatus::Playing,
            "no delay on an external clock"
        );
        p.step(Duration::from_secs(2));
        assert_eq!(p.time(), 0.0, "gameplay steps do nothing");
        p.set_external_time(1.0);
        assert_eq!(names(&p.take_fired()), ["a"]);
        p.set_external_time(0.5);
        assert!(p.take_fired().is_empty(), "backward is a seek");
        assert_eq!(p.time(), 0.5);
        p.set_external_time(9.0);
        assert_eq!(names(&p.take_fired()), ["a", "b"]);
        assert_eq!(p.status(), PlaybackStatus::Finished);
    }

    #[test]
    fn paused_during_the_start_delay_shows_nothing() {
        use crate::shots::asset::fixtures as shot_fixtures;
        let mut p = SequencePlayer::new(sequence(vec![], vec![]))
            .with_shots(shot_fixtures::sequence(vec![shot_fixtures::shot("a", 2.0)]));
        p.start_delay = 1.0;
        p.play();
        p.step(Duration::from_millis(200));
        p.pause();
        assert_eq!(p.status(), PlaybackStatus::Paused);
        assert!(!p.is_active(), "the clock never started");
        assert!(p.active_shot().is_none(), "so the camera is not claimed");
        p.play();
        p.step(Duration::from_secs(1));
        assert!(p.is_active() && p.active_shot().is_some());
        p.pause();
        assert!(p.is_active(), "paused after the delay holds its outputs");
    }

    #[test]
    fn an_external_loop_wraps_once_per_pass_of_absolute_time() {
        let mut p = SequencePlayer::new(sequence(
            vec![float_track("t", "ui", "opacity", 2.0)],
            vec![marker("mid", 1.0)],
        ));
        p.clock = ClockSource::External;
        p.end = EndPolicy::Loop;
        p.play();
        p.set_external_time(1.9);
        assert_eq!(names(&p.take_fired()), ["mid"]);
        p.set_external_time(2.1);
        assert_eq!(p.cycle(), 1);
        assert!((p.time() - 0.1).abs() < 1.0e-9);
        p.set_external_time(2.2);
        assert_eq!(p.cycle(), 1, "a write inside the same pass does not wrap again");
        assert!((p.time() - 0.2).abs() < 1.0e-9);
        assert!(p.take_fired().is_empty());
        p.set_external_time(3.0);
        assert_eq!(
            names(&p.take_fired()),
            ["mid"],
            "the next pass fires its marker once"
        );
    }

    #[test]
    fn play_from_after_the_end_is_a_fresh_start() {
        let mut p = playing(vec![marker("start", 0.0)], 2.0, EndPolicy::Hold);
        p.step(Duration::from_secs(3));
        assert_eq!(p.status(), PlaybackStatus::Finished);
        p.just_finished = false;
        p.finish_reported = true;
        p.take_fired();
        p.play_from(0.5);
        assert_eq!(p.status(), PlaybackStatus::Playing);
        assert!(p.start_requested, "a new start stamp is taken");
        assert!(!p.finish_reported, "the next end is reported again");
        assert!((p.time() - 0.5).abs() < 1.0e-9);
        p.step(Duration::from_millis(100));
        assert!(
            p.take_fired().is_empty(),
            "not from zero: the zero marker does not fire"
        );
        p.step(Duration::from_secs(5));
        assert_eq!(p.status(), PlaybackStatus::Finished);
        p.play_from(0.0);
        p.step(Duration::from_millis(100));
        assert_eq!(names(&p.take_fired()), ["start"], "from zero it does");
    }

    #[test]
    fn jumps_of_the_clock_count_as_shot_changes() {
        use crate::shots::asset::fixtures as shot_fixtures;
        let mut p = SequencePlayer::new(sequence(vec![], vec![]))
            .with_shots(shot_fixtures::sequence(vec![shot_fixtures::shot("a", 2.0)]));
        p.play();
        p.step(Duration::from_millis(500));
        assert!(!p.take_shot_change());
        p.seek(1.5);
        assert!(p.take_shot_change(), "a seek inside one shot still jumps");
        p.seek(1.5);
        assert!(!p.take_shot_change(), "a seek to the same time does not");
        p.step(Duration::from_secs(3));
        p.play();
        assert!(p.take_shot_change(), "a restart from the end jumps back");
    }

    #[test]
    fn stop_resets_and_stopped_players_are_inactive() {
        let mut p = playing(vec![], 2.0, EndPolicy::Hold);
        p.step(Duration::from_secs(1));
        p.stop();
        assert_eq!(p.time(), 0.0);
        assert_eq!(p.status(), PlaybackStatus::Stopped);
        assert!(!p.is_active());
        p.step(Duration::from_secs(1));
        assert_eq!(p.time(), 0.0, "a stopped clock does not run");
    }

    #[test]
    fn the_shot_list_shares_the_clock_and_sets_the_length() {
        use crate::shots::asset::fixtures as shot_fixtures;
        let shots =
            shot_fixtures::sequence(vec![shot_fixtures::shot("a", 2.0), shot_fixtures::shot("b", 3.0)]);
        let mut p = SequencePlayer::new(sequence(vec![float_track("t", "ui", "opacity", 1.0)], vec![]))
            .with_shots(shots);
        assert_eq!(p.duration(), 5.0, "the shot list is longer");
        p.play();
        assert_eq!(p.active_shot().map(|s| s.shot), Some(0));
        p.step(Duration::from_millis(2500));
        assert_eq!(p.active_shot().map(|s| s.shot), Some(1));
        assert!(p.shot_changed());
        assert_eq!(p.shot_changes(), 1);
        p.step(Duration::from_secs(9));
        assert_eq!(p.status(), PlaybackStatus::Finished);
        assert!(p.active_shot().is_some(), "hold keeps the last shot on screen");
        p.takeover.enabled = false;
        assert!(p.active_shot().is_none(), "takeover off: data only");
    }

    #[test]
    fn no_shot_is_active_past_the_end_of_a_shorter_shot_list() {
        use crate::shots::asset::fixtures as shot_fixtures;
        let shots = shot_fixtures::sequence(vec![shot_fixtures::shot("a", 1.0)]);
        let mut p = SequencePlayer::new(sequence(vec![float_track("t", "ui", "opacity", 3.0)], vec![]))
            .with_shots(shots);
        p.play();
        p.step(Duration::from_millis(500));
        assert!(p.active_shot().is_some());
        p.step(Duration::from_secs(1));
        assert!(
            p.active_shot().is_none(),
            "the camera falls back after the last shot"
        );
        assert!(p.is_active(), "the sequence itself still plays");
    }

    fn values_app(step: Duration) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(step))
            // The hot-path counters are thread-locals. The desktop executor runs systems on
            // the compute pool (ADR-4933), so a count read on this thread would miss them.
            .set_schedule_executor(regia_app::ScheduleExecutor::SingleThreaded);
        app
    }

    /// Pin: a one-second 60 Hz playback of a `0 → 90` yaw track writes the pinned euler
    /// quaternion at frame 30, bit for bit (spec 35 § 12.13).
    #[test]
    fn a_yaw_track_playback_pins_the_halfway_rotation_bits() {
        let mut app = values_app(Duration::from_nanos(16_666_667));
        let hero = app.world_mut().spawn(Transform::default()).id();
        let mut player = SequencePlayer::new(sequence(
            vec![Track {
                name: "turn".into(),
                target: "hero".into(),
                property: "transform.rotation".into(),
                keys: Channel::Vec3(vec![
                    Vec3Key::new(0.0, [0.0, 0.0, 0.0], Default::default()),
                    Vec3Key::new(1.0, [0.0, 90.0, 0.0], Default::default()),
                ]),
                enabled: true,
                locked: false,
            }],
            vec![],
        ));
        player.play();
        app.world_mut().spawn((
            player,
            SequenceBindings::default().with("hero", Binding::Entity(hero)),
        ));
        let mut at_half = None;
        for frame in 1..=30 {
            app.update();
            if frame == 30 {
                at_half = Some(app.world().get::<Transform>(hero).expect("hero").rotation);
            }
        }
        let q = at_half.expect("frame 30");
        assert_eq!(q.x.to_bits(), 0);
        assert_eq!(q.y.to_bits(), 0x3ec3_ef16);
        assert_eq!(q.z.to_bits(), 0);
        assert_eq!(q.w.to_bits(), 0x3f6c_835e);
    }

    /// A sequence moving `hero`, scaling `missing`, toggling `hero`'s visibility and keying an
    /// unregistered property.
    fn showcase() -> Sequence {
        sequence(
            vec![
                Track {
                    name: "move".into(),
                    target: "hero".into(),
                    property: "transform.position".into(),
                    keys: Channel::Vec3(vec![
                        Vec3Key::new(0.0, [0.0, 0.0, 0.0], Default::default()),
                        Vec3Key::new(2.0, [4.0, 0.0, 0.0], Default::default()),
                    ]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "show".into(),
                    target: "hero".into(),
                    property: "visible".into(),
                    keys: Channel::Bool(vec![
                        BoolKey {
                            time: 0.0,
                            value: false,
                        },
                        BoolKey {
                            time: 1.0,
                            value: true,
                        },
                    ]),
                    enabled: true,
                    locked: false,
                },
                float_track("grow", "missing", "transform.scale", 2.0),
                float_track("glow", "hero", "glow.amount", 2.0),
                float_track("wrong", "hero", "transform.position", 2.0),
            ],
            vec![marker("go", 0.0), marker("mid", 1.0)],
        )
    }

    #[derive(Resource, Default)]
    struct Seen(Vec<(String, u32)>);

    fn collect_markers(mut reader: MessageReader<MarkerReached>, mut seen: ResMut<Seen>) {
        for m in reader.read() {
            seen.0.push((m.name.clone(), m.cycle));
        }
    }

    #[test]
    fn bound_tracks_apply_in_order_and_problems_are_reported_once() {
        let mut app = values_app(Duration::from_millis(250));
        app.init_resource::<Seen>().add_systems(Update, collect_markers);
        let guid = Guid::from_u128(0x1234);
        let hero = app
            .world_mut()
            .spawn((Transform::default(), EntityGuid(guid)))
            .id();
        let mut player = SequencePlayer::new(showcase());
        player.play();
        let bindings = SequenceBindings::default().with("hero", Binding::Guid(guid));
        let p = app.world_mut().spawn((player, bindings)).id();
        for _ in 0..4 {
            app.update();
        }
        let player = app.world().get::<SequencePlayer>(p).expect("player");
        let t = player.time();
        assert!((t - 1.0).abs() < 1.0e-9, "{t}");
        let pose = app.world().get::<Transform>(hero).expect("hero");
        // `wrong` (a Float on a Vec3 property) is refused, so `move` stays applied.
        assert!(
            (pose.translation.x - 2.0).abs() < 1.0e-5,
            "{:?}",
            pose.translation
        );
        assert_eq!(app.world().get::<Visibility>(hero), Some(&Visibility::Visible));
        let issues: Vec<&str> = player.issues().map(SequenceIssue::kind).collect();
        assert_eq!(issues, ["unbound_target", "unknown_property", "apply_failed"]);
        app.update();
        let again = app
            .world()
            .get::<SequencePlayer>(p)
            .expect("player")
            .issues()
            .count();
        assert_eq!(again, 3, "each problem is reported once");
        let seen = &app.world().resource::<Seen>().0;
        assert_eq!(
            seen,
            &[("go".to_string(), 0), ("mid".to_string(), 0)],
            "markers arrive as messages"
        );
    }

    #[test]
    fn an_entity_binding_and_a_late_guid_both_resolve() {
        let mut app = values_app(Duration::from_millis(100));
        let hero = app.world_mut().spawn(Transform::default()).id();
        let mut player = SequencePlayer::new(showcase());
        player.play();
        let late = Guid::from_u128(77);
        let p = app
            .world_mut()
            .spawn((
                player,
                SequenceBindings::default()
                    .with("hero", Binding::Entity(hero))
                    .with("missing", Binding::Guid(late)),
            ))
            .id();
        app.update();
        let player = app.world().get::<SequencePlayer>(p).expect("player");
        assert_eq!(player.resolved_targets()[0], Some(hero));
        assert_eq!(player.resolved_targets()[2], None);
        let crate_entity = app
            .world_mut()
            .spawn((Transform::default(), EntityGuid(late)))
            .id();
        app.update();
        let player = app.world().get::<SequencePlayer>(p).expect("player");
        assert_eq!(
            player.resolved_targets()[2],
            Some(crate_entity),
            "the index rebuilds on a new guid"
        );
        let scale = app.world().get::<Transform>(crate_entity).expect("crate").scale;
        assert!(
            scale.x > 0.0 && scale.x < 1.0,
            "a Float scales uniformly: {scale:?}"
        );
    }

    #[test]
    fn the_release_overshoot_counts_from_the_end_of_a_shorter_shot_list() {
        use crate::shots::asset::fixtures as shot_fixtures;
        // 1.875 s and 250 ms steps are exact in binary: the finishing step goes from 1.75 to 2.0.
        let mut app = values_app(Duration::from_millis(250));
        let mut player = SequencePlayer::new(sequence(vec![float_track("t", "ui", "opacity", 2.0)], vec![]))
            .with_shots(shot_fixtures::sequence(vec![shot_fixtures::shot("a", 1.875)]));
        player.end = EndPolicy::Once;
        player.play();
        let p = app.world_mut().spawn(player).id();
        let mut released = Vec::new();
        for _ in 0..12 {
            app.update();
            let player = app.world().get::<SequencePlayer>(p).expect("player");
            if let Some(overshoot) = player.release_overshoot() {
                released.push((player.time(), overshoot));
            }
        }
        assert_eq!(released.len(), 1, "{released:?}");
        assert_eq!(released[0].0, 2.0, "released on the finishing step");
        assert_eq!(
            released[0].1,
            Duration::from_millis(125),
            "the shots ended at 1.875 s"
        );
    }

    /// audit-seq (Medium): an external clock written before `SequencerSet::Advance` (render-out's
    /// `drive_job_sequences`) crossed the shots' end before the advance looked, so the release
    /// overshoot was never set and the blend out fell by the whole frame. Same numbers as the
    /// gameplay-clock test above: the finishing write goes from 1.75 s to 2.0 s, the shots end at
    /// 1.875 s.
    #[test]
    fn an_external_clock_written_before_the_advance_reports_the_release_overshoot() {
        use crate::shots::asset::fixtures as shot_fixtures;
        #[derive(Resource, Default)]
        struct HostTime(f64);
        fn write_external(mut host: ResMut<HostTime>, mut players: Query<&mut SequencePlayer>) {
            host.0 += 0.25;
            for mut player in &mut players {
                player.set_external_time(host.0);
            }
        }
        let mut app = values_app(Duration::from_millis(250));
        app.init_resource::<HostTime>()
            .add_systems(PostUpdate, write_external.before(SequencerSet::Advance));
        let mut player = SequencePlayer::new(sequence(vec![float_track("t", "ui", "opacity", 2.0)], vec![]))
            .with_shots(shot_fixtures::sequence(vec![shot_fixtures::shot("a", 1.875)]));
        player.end = EndPolicy::Once;
        player.clock = ClockSource::External;
        player.play();
        let p = app.world_mut().spawn(player).id();
        let mut released = Vec::new();
        for _ in 0..12 {
            app.update();
            let player = app.world().get::<SequencePlayer>(p).expect("player");
            if let Some(overshoot) = player.release_overshoot() {
                released.push((player.time(), overshoot));
            }
        }
        assert_eq!(released.len(), 1, "{released:?}");
        assert_eq!(released[0].0, 2.0, "released on the finishing write");
        assert_eq!(
            released[0].1,
            Duration::from_millis(125),
            "the shots ended at 1.875 s"
        );
    }

    #[test]
    fn a_target_that_binds_later_is_no_longer_reported() {
        let mut app = values_app(Duration::from_millis(100));
        let mut seq = showcase();
        seq.tracks.truncate(1);
        let mut player = SequencePlayer::new(seq);
        player.play();
        let p = app.world_mut().spawn((player, SequenceBindings::default())).id();
        app.update();
        let kinds = |app: &App| {
            app.world()
                .get::<SequencePlayer>(p)
                .expect("player")
                .issues()
                .map(SequenceIssue::kind)
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(&app), ["unbound_target"]);
        let hero = app.world_mut().spawn(Transform::default()).id();
        app.world_mut()
            .get_mut::<SequenceBindings>(p)
            .expect("bindings")
            .bind("hero", Binding::Entity(hero));
        app.update();
        assert!(kinds(&app).is_empty(), "bound now");
        let guid = Guid::from_u128(5);
        let gone = app.world_mut().spawn(EntityGuid(guid)).id();
        app.world_mut()
            .get_mut::<SequenceBindings>(p)
            .expect("bindings")
            .bind("hero", Binding::Guid(guid));
        app.update();
        app.world_mut().despawn(gone);
        app.update();
        assert_eq!(
            app.world()
                .get::<SequencePlayer>(p)
                .expect("player")
                .resolved_targets()[0],
            None,
            "a despawned guid target is unbound"
        );
    }

    #[test]
    fn stopped_and_waiting_players_apply_nothing() {
        let mut app = values_app(Duration::from_millis(100));
        let hero = app
            .world_mut()
            .spawn(Transform {
                translation: Vec3::new(9.0, 9.0, 9.0),
                ..Transform::default()
            })
            .id();
        let mut player = SequencePlayer::new(showcase());
        player.start_delay = 10.0;
        player.play();
        app.world_mut().spawn((
            player,
            SequenceBindings::default().with("hero", Binding::Entity(hero)),
        ));
        let stopped = SequencePlayer::new(showcase());
        app.world_mut().spawn((
            stopped,
            SequenceBindings::default().with("hero", Binding::Entity(hero)),
        ));
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(
            app.world().get::<Transform>(hero).expect("hero").translation,
            Vec3::new(9.0, 9.0, 9.0)
        );
    }

    #[test]
    fn host_appliers_extend_the_registry() {
        fn halve(entity: &mut EntityWorldMut<'_>, value: ChannelValue) -> Result<(), ApplyError> {
            let ChannelValue::Float(v) = value else {
                return Err(ApplyError::WrongKind { expected: "Float" });
            };
            entity
                .get_mut::<Transform>()
                .ok_or(ApplyError::MissingComponent("Transform"))?
                .translation
                .y = v * 0.5;
            Ok(())
        }
        let mut app = values_app(Duration::from_millis(500));
        app.world_mut()
            .resource_mut::<PropertyAppliers>()
            .register("glow.amount", halve);
        assert!(
            app.world()
                .resource::<PropertyAppliers>()
                .paths()
                .contains(&"glow.amount")
        );
        let hero = app.world_mut().spawn(Transform::default()).id();
        let mut player = SequencePlayer::new(sequence(
            vec![float_track("glow", "hero", "glow.amount", 2.0)],
            vec![],
        ));
        player.play();
        app.world_mut().spawn((
            player,
            SequenceBindings::default().with("hero", Binding::Entity(hero)),
        ));
        for _ in 0..2 {
            app.update();
        }
        let y = app.world().get::<Transform>(hero).expect("hero").translation.y;
        assert!((y - 0.25).abs() < 1.0e-6, "{y}");
    }

    #[test]
    fn built_in_appliers_write_rotation_lights_and_fov() {
        let mut world = World::new();
        let e = world
            .spawn((Transform::default(), PointLight::default(), Camera::default()))
            .id();
        let mut entity = world.entity_mut(e);
        apply_rotation(&mut entity, ChannelValue::Vec3([0.0, 90.0, 0.0])).expect("rotation");
        apply_light_intensity(&mut entity, ChannelValue::Float(800.0)).expect("intensity");
        apply_light_color(&mut entity, ChannelValue::Vec3([1.0, 0.5, 0.25])).expect("color");
        apply_camera_fov(&mut entity, ChannelValue::Float(30.0)).expect("fov");
        assert_eq!(
            apply_visible(&mut entity, ChannelValue::Float(1.0)),
            Err(ApplyError::WrongKind { expected: "Bool" })
        );
        let rotation = world.get::<Transform>(e).expect("transform").rotation;
        let forward = rotation * Vec3::NEG_Z;
        assert!(
            (forward - Vec3::NEG_X).length() < 1.0e-5,
            "yaw 90 turns towards -X: {forward:?}"
        );
        // Pin: the rotation is the pinned euler construction (yaw 90 degrees), bit for bit, not
        // the platform libm's `sin`/`cos` (spec 35 § 12.13 class).
        assert_eq!(rotation.x.to_bits(), 0);
        assert_eq!(rotation.y.to_bits(), 0x3f35_04f3);
        assert_eq!(rotation.z.to_bits(), 0);
        assert_eq!(rotation.w.to_bits(), 0x3f35_04f3);
        let light = world.get::<PointLight>(e).expect("light");
        assert_eq!(light.intensity, 800.0);
        assert_eq!(light.color, [1.0, 0.5, 0.25]);
        let fov = world.get::<Camera>(e).expect("camera").fov_y;
        assert!((fov - 30.0f32.to_radians()).abs() < 1.0e-6);
        let bare = world.spawn_empty().id();
        let mut bare = world.entity_mut(bare);
        assert_eq!(
            apply_light_color(&mut bare, ChannelValue::Vec3([1.0; 3])),
            Err(ApplyError::MissingComponent("light"))
        );
    }

    #[test]
    fn once_applies_its_last_frame_once_and_then_leaves_the_target_alone() {
        let mut app = values_app(Duration::from_millis(500));
        let hero = app.world_mut().spawn(Transform::default()).id();
        let mut seq = showcase();
        seq.tracks.truncate(1);
        let mut player = SequencePlayer::new(seq);
        player.end = EndPolicy::Once;
        player.play();
        let p = app
            .world_mut()
            .spawn((
                player,
                SequenceBindings::default().with("hero", Binding::Entity(hero)),
            ))
            .id();
        for _ in 0..6 {
            app.update();
        }
        assert_eq!(
            app.world().get::<SequencePlayer>(p).expect("player").status(),
            PlaybackStatus::Finished
        );
        let end = app.world().get::<Transform>(hero).expect("hero").translation;
        assert_eq!(end, Vec3::new(4.0, 0.0, 0.0), "the end frame was applied");
        app.world_mut()
            .get_mut::<Transform>(hero)
            .expect("hero")
            .translation = Vec3::new(-1.0, 0.0, 0.0);
        app.update();
        assert_eq!(
            app.world().get::<Transform>(hero).expect("hero").translation,
            Vec3::new(-1.0, 0.0, 0.0),
            "gameplay owns the target again"
        );
    }

    #[test]
    fn an_external_clock_in_an_app_sends_the_markers_it_crosses() {
        let mut app = values_app(Duration::from_millis(100));
        app.init_resource::<Seen>().add_systems(Update, collect_markers);
        let mut player = SequencePlayer::new(showcase());
        player.clock = ClockSource::External;
        player.play();
        let p = app.world_mut().spawn(player).id();
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world().get::<SequencePlayer>(p).expect("player").time(),
            0.0,
            "game time does not move an external clock"
        );
        app.world_mut()
            .get_mut::<SequencePlayer>(p)
            .expect("player")
            .set_external_time(1.25);
        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<Seen>().0,
            [("go".to_string(), 0), ("mid".to_string(), 0)]
        );
    }

    /// Runs a looping sequence at a fixed step and records every marker message and the value
    /// at every whole 1/30 s.
    fn replay(step: Duration, frames: usize, every: usize) -> (Vec<(String, u32)>, Vec<[u32; 3]>) {
        let mut app = values_app(step);
        app.init_resource::<Seen>().add_systems(Update, collect_markers);
        let hero = app.world_mut().spawn(Transform::default()).id();
        let mut seq = showcase();
        seq.markers.push(marker("late", 1.9));
        seq.markers.push(marker("also_mid", 1.0));
        let mut player = SequencePlayer::new(seq);
        player.end = EndPolicy::Loop;
        player.play();
        app.world_mut().spawn((
            player,
            SequenceBindings::default().with("hero", Binding::Entity(hero)),
        ));
        let mut values = Vec::new();
        for frame in 1..=frames {
            app.update();
            if frame % every == 0 {
                let t = app.world().get::<Transform>(hero).expect("hero").translation;
                values.push([t.x.to_bits(), t.y.to_bits(), t.z.to_bits()]);
            }
        }
        // Markers written in the last frame's PostUpdate are read by the next frame's Update.
        app.update();
        (app.world().resource::<Seen>().0.clone(), values)
    }

    #[test]
    fn replays_at_30_and_60_hz_fixed_steps_are_identical() {
        // 1/30 s and 1/60 s as whole nanoseconds, so two 60 Hz steps are exactly one 30 Hz step.
        let thirty = Duration::from_nanos(33_333_334);
        let sixty = Duration::from_nanos(16_666_667);
        let (markers_30, values_30) = replay(thirty, 150, 1);
        let (markers_60, values_60) = replay(sixty, 300, 2);
        assert_eq!(
            values_30, values_60,
            "bit-identical values at every shared instant"
        );
        // The 60 Hz run takes one more frame for its last 30 Hz instant's markers.
        assert_eq!(markers_30, markers_60, "the same markers in the same order");
        assert!(markers_30.iter().any(|(_, cycle)| *cycle >= 2), "the run wraps");
        let (again, values_again) = replay(thirty, 150, 1);
        assert_eq!(
            (again, values_again),
            (markers_30, values_30),
            "a replay is identical"
        );
    }

    /// Thirty-two bound names keep their `String`s on the second advance.
    #[test]
    fn bound_targets_keep_their_storage_across_frames() {
        let mut app = values_app(Duration::from_millis(50));
        let mut bindings = SequenceBindings::default();
        let mut ids = Vec::new();
        let mut tracks = Vec::new();
        for i in 0..32 {
            let name = format!("t{i:02}");
            let entity = app.world_mut().spawn(Transform::default()).id();
            ids.push((name.clone(), entity));
            bindings = bindings.with(name.clone(), Binding::Entity(entity));
            tracks.push(float_track(&format!("n{i:02}"), &name, "transform.scale", 1.0));
        }
        let mut player = SequencePlayer::new(sequence(tracks, vec![]));
        player.play();
        let p = app.world_mut().spawn((player, bindings)).id();
        app.update();
        hot_counts::reset();
        app.update();
        assert_eq!(
            hot_counts::target(),
            0,
            "the second frame cloned {} target names",
            hot_counts::target()
        );
        let stored = app.world().get::<SequencePlayer>(p).expect("player");
        for (name, entity) in &ids {
            assert_eq!(stored.target_entities().get(name), Some(entity), "{name}");
        }
    }

    /// Successful writes do not copy track names; an apply error still names the track.
    #[test]
    fn apply_skips_track_name_strings_when_every_write_succeeds() {
        let mut app = values_app(Duration::from_millis(100));
        let hero = app.world_mut().spawn(Transform::default()).id();
        let seq = sequence(
            vec![
                Track {
                    name: "move".into(),
                    target: "hero".into(),
                    property: "transform.position".into(),
                    keys: Channel::Vec3(vec![
                        Vec3Key::new(0.0, [0.0, 0.0, 0.0], Default::default()),
                        Vec3Key::new(1.0, [2.0, 0.0, 0.0], Default::default()),
                    ]),
                    enabled: true,
                    locked: false,
                },
                float_track("grow", "hero", "transform.scale", 1.0),
            ],
            vec![],
        );
        let mut player = SequencePlayer::new(seq);
        player.play();
        let p = app
            .world_mut()
            .spawn((
                player,
                SequenceBindings::default().with("hero", Binding::Entity(hero)),
            ))
            .id();
        app.update();
        hot_counts::reset();
        app.update();
        assert_eq!(
            hot_counts::track(),
            0,
            "the second successful frame copied {} track names",
            hot_counts::track()
        );
        let x = app.world().get::<Transform>(hero).expect("hero").translation.x;
        assert!(x > 0.0, "the position track applied, x = {x}");
        app.world_mut().entity_mut(hero).remove::<Transform>();
        app.update();
        let player = app.world().get::<SequencePlayer>(p).expect("player");
        let text: Vec<String> = player.issues().map(|issue| issue.to_string()).collect();
        assert!(
            text.iter().any(|line| line.contains("move")),
            "the apply error still names the track: {text:?}"
        );
    }

    /// Crossing one marker in a long sorted list looks at a window, not every marker.
    #[test]
    fn marker_collect_checks_a_window_not_the_whole_list() {
        const N: u32 = 4096;
        let markers: Vec<_> = (0..N).map(|i| marker(&format!("m{i:04}"), i as f32)).collect();
        let mut player = SequencePlayer::new(sequence(vec![], markers));
        player.play();
        player.seek(2047.5);
        hot_counts::reset();
        player.step(Duration::from_secs(1));
        let fired = player.take_fired();
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert_eq!(fired[0].name, "m2048");
        let bound = u64::from(N.ilog2()) * 2 + 4;
        assert!(
            hot_counts::marker() <= bound,
            "collect visited {} markers, bound {bound}",
            hot_counts::marker()
        );
    }

    // ---- `.seq.ron` v2 (ADR-1880): clips, sub-sequences, audio ----

    mod v2 {
        use std::sync::Arc;

        use regia_animation::clip::{JointTrack, TrackCurve};
        use regia_animation::pose::translated;
        use regia_animation::skeleton::Skeleton;
        use regia_animation::{AnimationClip, JointGlobals, components::spawn_skeleton};
        use regia_audio::{Audio, AudioSettings, SoundAsset, SoundAssets, SoundClip, SoundInstanceId};

        use super::super::super::asset::{AudioSection, ClipSection, Ease, SubSection, TimeScaleMode};
        use super::super::super::audio::{SequenceVoices, fade_db};
        use super::super::super::sample::SequenceLibrary;
        use super::*;

        /// A three-joint chain with its bind at x = 0.
        fn rig(world: &mut World) -> (Entity, Vec<Entity>) {
            let names = (0..3).map(|i| format!("j{i}")).collect();
            let parents = (0..3)
                .map(|i: usize| i.checked_sub(1).map(|p| p as u32))
                .collect();
            let bind = (0..3).map(|_| translated(Vec3::new(0.0, 0.1, 0.0))).collect();
            let skeleton = Arc::new(Skeleton::from_local_bind(names, parents, bind).expect("skeleton"));
            let root = world.spawn(Transform::default()).id();
            let binding = spawn_skeleton(world, skeleton);
            let joints = binding.joints.clone();
            world.entity_mut(root).insert(binding);
            (root, joints)
        }

        /// A clip holding joint 1 at `x`.
        fn hold(name: &str, x: f32) -> AnimationClip {
            let mut clip = AnimationClip::empty(name, 1.0);
            clip.tracks.push(JointTrack {
                joint: 1,
                translations: TrackCurve::Static(Vec3::new(x, 0.1, 0.0)),
                rotations: TrackCurve::Static(Quat::IDENTITY),
                scales: TrackCurve::Static(Vec3::ONE),
            });
            clip
        }

        fn section(clip: &str, start: f32, end: f32, blend_in: f32, blend_out: f32) -> ClipSection {
            ClipSection {
                clip: clip.into(),
                start,
                end,
                offset: 0.0,
                speed: 1.0,
                looping: false,
                blend_in,
                blend_out,
                weight: 1.0,
            }
        }

        fn track(name: &str, target: &str, keys: Channel) -> Track {
            Track {
                name: name.into(),
                target: target.into(),
                property: String::new(),
                keys,
                enabled: true,
                locked: false,
            }
        }

        fn joint_x(app: &App, joint: Entity) -> f32 {
            app.world().get::<Transform>(joint).expect("joint").translation.x
        }

        #[test]
        fn clip_sections_blend_onto_the_rig_and_untouched_joints_keep_their_pose() {
            let mut app = values_app(Duration::from_millis(250));
            {
                let mut clips = app.world_mut().resource_mut::<SequenceClips>();
                clips.insert("left", hold("left", 2.0));
                clips.insert("right", hold("right", 4.0));
            }
            let (root, joints) = rig(app.world_mut());
            app.world_mut()
                .get_mut::<Transform>(joints[2])
                .expect("j2")
                .translation
                .x = 7.0;
            let seq = sequence(
                vec![track(
                    "body",
                    "hero",
                    Channel::Clip(vec![
                        section("left", 0.0, 2.0, 0.0, 1.0),
                        section("right", 1.0, 3.0, 1.0, 0.0),
                    ]),
                )],
                vec![],
            );
            let mut player = SequencePlayer::new(seq);
            player.end = EndPolicy::Hold;
            player.play();
            let bindings = SequenceBindings::default().with("hero", Binding::Entity(root));
            let p = app.world_mut().spawn((player, bindings)).id();
            let mut xs = Vec::new();
            for _ in 0..12 {
                app.update();
                xs.push(joint_x(&app, joints[1]));
            }
            // 0.25 .. 1.0: left alone (2); 1.0 .. 2.0: a linear crossfade; then right (4).
            assert_eq!(xs[0], 2.0);
            assert_eq!(xs[3], 2.0, "t = 1.0: right's blend in is still at zero");
            assert!((xs[5] - 3.0).abs() < 1.0e-5, "t = 1.5: half and half, {}", xs[5]);
            assert!(
                (xs[7] - 4.0).abs() < 1.0e-5,
                "t = 2.0: left has blended out, {}",
                xs[7]
            );
            assert_eq!(xs[11], 4.0, "held at the end");
            for pair in xs.windows(2) {
                assert!(pair[1] >= pair[0] - 1.0e-6, "the crossfade is monotone: {xs:?}");
            }
            assert_eq!(
                joint_x(&app, joints[2]),
                7.0,
                "a joint no clip animates is left alone"
            );
            let player = app.world().get::<SequencePlayer>(p).expect("player");
            assert_eq!(
                player.issues().count(),
                0,
                "{:?}",
                player.issues().collect::<Vec<_>>()
            );
        }

        #[test]
        fn a_blend_in_starts_from_the_rigs_own_pose_and_problems_are_reported() {
            let mut app = values_app(Duration::from_millis(250));
            app.world_mut()
                .resource_mut::<SequenceClips>()
                .insert("wave", hold("wave", 4.0));
            let (root, joints) = rig(app.world_mut());
            app.world_mut()
                .get_mut::<Transform>(joints[1])
                .expect("j1")
                .translation
                .x = 1.0;
            let no_rig = app.world_mut().spawn(Transform::default()).id();
            let seq = sequence(
                vec![
                    track(
                        "in",
                        "hero",
                        Channel::Clip(vec![section("wave", 0.0, 2.0, 1.0, 0.0)]),
                    ),
                    track(
                        "bad",
                        "hero",
                        Channel::Clip(vec![section("nope", 0.0, 2.0, 0.0, 0.0)]),
                    ),
                    track(
                        "busy",
                        "busy",
                        Channel::Clip(vec![section("wave", 0.0, 2.0, 0.0, 0.0)]),
                    ),
                ],
                vec![],
            );
            let mut player = SequencePlayer::new(seq);
            player.play();
            let bindings = SequenceBindings::default()
                .with("hero", Binding::Entity(root))
                .with("busy", Binding::Entity(no_rig));
            let p = app.world_mut().spawn((player, bindings)).id();
            app.update();
            // t = 0.25: weight 0.25 over the rig's own x = 1 → 1.75.
            assert!(
                (joint_x(&app, joints[1]) - 1.75).abs() < 1.0e-5,
                "{}",
                joint_x(&app, joints[1])
            );
            let player = app.world().get::<SequencePlayer>(p).expect("player");
            let kinds: Vec<&str> = player.issues().map(SequenceIssue::kind).collect();
            assert_eq!(
                kinds,
                ["apply_failed", "unknown_clip"],
                "{:?}",
                player.issues().collect::<Vec<_>>()
            );
            assert!(player.issues().any(|i| i.to_string().contains("SkinnedMesh")));
        }

        /// The x of joint 1 after each of `frames` updates of a one-section blend over a rig whose
        /// own x is 1, at `step`.
        fn blended(step: Duration, frames: usize) -> Vec<f32> {
            let mut app = values_app(step);
            app.world_mut()
                .resource_mut::<SequenceClips>()
                .insert("wave", hold("wave", 4.0));
            let (root, joints) = rig(app.world_mut());
            app.world_mut()
                .get_mut::<Transform>(joints[1])
                .expect("j1")
                .translation
                .x = 1.0;
            let seq = sequence(
                vec![track(
                    "wave",
                    "hero",
                    Channel::Clip(vec![section("wave", 0.0, 2.0, 1.0, 1.0)]),
                )],
                vec![],
            );
            let mut player = SequencePlayer::new(seq);
            player.play();
            let bindings = SequenceBindings::default().with("hero", Binding::Entity(root));
            app.world_mut().spawn((player, bindings));
            (0..frames)
                .map(|_| {
                    app.update();
                    joint_x(&app, joints[1])
                })
                .collect()
        }

        #[test]
        fn a_blend_does_not_depend_on_the_frame_rate_and_returns_to_the_rigs_own_pose() {
            let coarse = blended(Duration::from_millis(250), 8);
            let fine = blended(Duration::from_millis(50), 40);
            // t = 0.5: weight 0.5 from 1 toward 4.
            assert!((coarse[1] - 2.5).abs() < 1.0e-5, "{coarse:?}");
            for (i, x) in coarse.iter().enumerate() {
                let same = fine[(i + 1) * 5 - 1];
                assert!((x - same).abs() < 1.0e-4, "t = {}: {x} and {same}", (i + 1) * 250);
            }
            assert!((coarse[3] - 4.0).abs() < 1.0e-5, "full weight at 1.0: {coarse:?}");
            assert_eq!(coarse[7], 1.0, "the blend out ends on the rig's own pose");
        }

        #[test]
        fn a_stopped_or_despawned_player_gives_the_rig_back_and_outside_moves_count() {
            let mut app = values_app(Duration::from_millis(250));
            app.world_mut()
                .resource_mut::<SequenceClips>()
                .insert("wave", hold("wave", 4.0));
            let (root, joints) = rig(app.world_mut());
            let x = |app: &mut App, value: f32| {
                app.world_mut()
                    .get_mut::<Transform>(joints[1])
                    .expect("j1")
                    .translation
                    .x = value;
            };
            x(&mut app, 1.0);
            let seq = sequence(
                vec![track(
                    "wave",
                    "hero",
                    Channel::Clip(vec![section("wave", 0.0, 4.0, 0.0, 0.0)]),
                )],
                vec![],
            );
            let mut player = SequencePlayer::new(seq.clone());
            player.play();
            let bindings = SequenceBindings::default().with("hero", Binding::Entity(root));
            let p = app.world_mut().spawn((player, bindings)).id();
            app.update();
            assert_eq!(joint_x(&app, joints[1]), 4.0);
            app.world_mut().get_mut::<SequencePlayer>(p).expect("p").stop();
            app.update();
            assert_eq!(joint_x(&app, joints[1]), 1.0, "stopped: its own pose");

            // A half-weight clip over a pose something else moves.
            let mut half = seq;
            let Channel::Clip(sections) = &mut half.tracks[0].keys else {
                panic!("clip")
            };
            sections[0].weight = 0.5;
            app.world_mut()
                .get_mut::<SequencePlayer>(p)
                .expect("p")
                .replace_sequence(half);
            app.world_mut().get_mut::<SequencePlayer>(p).expect("p").play();
            app.update();
            assert_eq!(joint_x(&app, joints[1]), 2.5);
            x(&mut app, 2.0);
            app.update();
            assert_eq!(joint_x(&app, joints[1]), 3.0, "the new pose is its own now");
            app.world_mut().despawn(p);
            app.update();
            assert_eq!(joint_x(&app, joints[1]), 2.0, "despawned: its own pose");
        }

        #[test]
        fn two_clip_tracks_on_one_rig_stack_in_track_order() {
            let mut app = values_app(Duration::from_millis(250));
            {
                let mut clips = app.world_mut().resource_mut::<SequenceClips>();
                clips.insert("a", hold("a", 2.0));
                clips.insert("b", hold("b", 4.0));
            }
            let (root, joints) = rig(app.world_mut());
            let half = |clip: &str| {
                let mut s = section(clip, 0.0, 4.0, 0.0, 0.0);
                s.weight = 0.5;
                Channel::Clip(vec![s])
            };
            let seq = sequence(
                vec![track("a", "hero", half("a")), track("b", "hero", half("b"))],
                vec![],
            );
            let mut player = SequencePlayer::new(seq);
            player.play();
            let bindings = SequenceBindings::default().with("hero", Binding::Entity(root));
            app.world_mut().spawn((player, bindings));
            for _ in 0..3 {
                app.update();
                // Own 0, a at half: 1; b at half over that: 2.5; the same every frame.
                assert!((joint_x(&app, joints[1]) - 2.5).abs() < 1.0e-5);
            }
        }

        #[test]
        fn a_rig_with_an_animator_blends_over_its_evaluated_pose_and_the_skin_follows() {
            let mut app = values_app(Duration::from_millis(250));
            app.add_plugins(regia_animation::AnimationPlugin);
            app.world_mut()
                .resource_mut::<SequenceClips>()
                .insert("wave", hold("wave", 4.0));
            let (root, joints) = rig(app.world_mut());
            app.world_mut()
                .entity_mut(root)
                .insert(regia_animation::Animator::single(hold("idle", 2.0), true));
            let seq = sequence(
                vec![track(
                    "wave",
                    "hero",
                    Channel::Clip(vec![section("wave", 0.0, 2.0, 1.0, 1.0)]),
                )],
                vec![],
            );
            let mut player = SequencePlayer::new(seq);
            player.play();
            let bindings = SequenceBindings::default().with("hero", Binding::Entity(root));
            let p = app.world_mut().spawn((player, bindings)).id();
            let joint = |app: &App| joint_x(app, joints[1]);
            let global = |app: &App| {
                let globals = app.world().get::<JointGlobals>(root).expect("globals");
                globals.globals[1].w_axis.x
            };
            app.update();
            // t = 0.25: weight 0.25 of 4 over the animator's 2.
            assert!((joint(&app) - 2.5).abs() < 1.0e-5, "{}", joint(&app));
            assert!(
                (global(&app) - 2.5).abs() < 1.0e-5,
                "the palette's pose: {}",
                global(&app)
            );
            app.update();
            app.update();
            app.update();
            // t = 1: full weight.
            assert!((joint(&app) - 4.0).abs() < 1.0e-5, "{}", joint(&app));
            assert!((global(&app) - 4.0).abs() < 1.0e-5, "{}", global(&app));
            for _ in 0..5 {
                app.update();
            }
            // Past the end the animator has its rig back, with nothing left of the blend.
            let player = app.world().get::<SequencePlayer>(p).expect("player");
            assert_eq!(player.issues().count(), 0);
            assert!(player.time() >= 2.0);
            assert!((joint(&app) - 2.0).abs() < 1.0e-5, "{}", joint(&app));
        }

        #[test]
        fn looping_clip_time_wraps_and_a_held_clip_clamps() {
            assert_eq!(wrap_clip_time(2.5, 1.0, true), 0.5);
            assert_eq!(wrap_clip_time(-0.25, 1.0, true), 0.75);
            assert_eq!(wrap_clip_time(2.5, 1.0, false), 1.0);
            assert_eq!(wrap_clip_time(-1.0, 1.0, false), 0.0);
            assert_eq!(wrap_clip_time(3.0, 0.0, true), 0.0, "a zero-length clip");
        }

        fn sub(sequence: &str, start: f32, end: f32) -> SubSection {
            SubSection {
                sequence: sequence.into(),
                start,
                end,
                offset: 0.0,
                speed: 1.0,
                bindings: [("lamp".to_string(), "hero".to_string())].into(),
                repeat: Default::default(),
                loop_length: None,
            }
        }

        #[test]
        fn sub_sequences_apply_and_fire_markers_on_the_parent_clock_and_follow_library_edits() {
            let mut app = values_app(Duration::from_millis(500));
            app.init_resource::<Seen>().add_systems(Update, collect_markers);
            let inner = |x: f32| {
                let mut seq = sequence(vec![], vec![marker("flash", 1.0)]);
                seq.tracks.push(Track {
                    name: "slide".into(),
                    target: "lamp".into(),
                    property: "transform.position".into(),
                    keys: Channel::Vec3(vec![
                        Vec3Key::new(0.0, [0.0, 0.0, 0.0], Ease::Linear),
                        Vec3Key::new(2.0, [x, 0.0, 0.0], Ease::Linear),
                    ]),
                    enabled: true,
                    locked: false,
                });
                seq
            };
            app.world_mut()
                .resource_mut::<SequenceLibrary>()
                .insert("fx", inner(4.0));
            let hero = app.world_mut().spawn(Transform::default()).id();
            let parent = sequence(
                vec![track("fx", "", Channel::Sub(vec![sub("fx", 1.0, 3.0)]))],
                vec![marker("go", 0.0)],
            );
            let mut player = SequencePlayer::new(parent);
            player.play();
            let p = app
                .world_mut()
                .spawn((
                    player,
                    SequenceBindings::default().with("hero", Binding::Entity(hero)),
                ))
                .id();
            for _ in 0..4 {
                app.update();
            }
            // t = 2.0 on the parent is 1.0 in the sub-sequence: x = 2.
            let x = app.world().get::<Transform>(hero).expect("hero").translation.x;
            assert!((x - 2.0).abs() < 1.0e-5, "{x}");
            let player = app.world().get::<SequencePlayer>(p).expect("player");
            assert_eq!(player.target_names(), ["hero"]);
            assert_eq!(player.target_entities().get("hero"), Some(&hero));
            assert_eq!(player.issues().count(), 0);
            // A library edit reaches the playing player.
            app.world_mut()
                .resource_mut::<SequenceLibrary>()
                .insert("fx", inner(8.0));
            app.update();
            let x = app.world().get::<Transform>(hero).expect("hero").translation.x;
            assert!((x - 6.0).abs() < 1.0e-5, "t = 2.5 → sub 1.5 of an 8 m slide: {x}");
            // `Update` reads what the last `PostUpdate` wrote: the marker at 2.0 is here by now.
            let seen = &app.world().resource::<Seen>().0;
            assert_eq!(seen, &[("go".to_string(), 0), ("fx/flash".to_string(), 0)]);
            // Removing it is reported once.
            app.world_mut().resource_mut::<SequenceLibrary>().remove("fx");
            app.update();
            let kinds: Vec<&str> = app
                .world()
                .get::<SequencePlayer>(p)
                .expect("player")
                .issues()
                .map(SequenceIssue::kind)
                .collect();
            assert!(kinds.contains(&"sub_sequence"), "{kinds:?}");
        }

        fn tone(seconds: f32) -> SoundAsset {
            let samples: Vec<f32> = (0..(seconds * 8_000.0) as usize)
                .map(|i| ((i % 40) as f32 / 40.0) - 0.5)
                .collect();
            SoundAsset::from_clip(&SoundClip::new(samples, 8_000).expect("clip"))
        }

        fn audio_app() -> App {
            let mut app = values_app(Duration::from_millis(500));
            app.insert_resource(Audio::new(&AudioSettings {
                headless: true,
                ..AudioSettings::default()
            }));
            let mut sounds = SoundAssets::default();
            sounds.add("theme", tone(4.0));
            app.insert_resource(sounds);
            app
        }

        fn music(sound: &str, start: f32, end: f32) -> Track {
            track(
                "music",
                "",
                Channel::Audio(vec![AudioSection {
                    sound: sound.into(),
                    start,
                    end,
                    offset: 0.5,
                    volume_db: -3.0,
                    fade_in: 0.0,
                    fade_out: 1.0,
                    bus: None,
                    time_scale: 1.0,
                    spatial: None,
                    time_scale_mode: TimeScaleMode::Varispeed,
                }]),
            )
        }

        #[test]
        fn audio_sections_start_restart_on_a_seek_and_stop_their_voices() {
            let mut app = audio_app();
            let mut player =
                SequencePlayer::new(sequence(vec![music("theme", 1.0, 3.0)], vec![marker("end", 4.0)]));
            player.play();
            let p = app.world_mut().spawn(player).id();
            let counts = |app: &App| {
                let v = app.world().resource::<SequenceVoices>();
                (v.voices(p).len(), v.started(), v.stopped())
            };
            app.update(); // t = 0.5
            assert_eq!(counts(&app), (0, 0, 0), "before the section");
            app.update(); // t = 1.0
            assert_eq!(counts(&app), (1, 1, 0), "the section started a voice");
            let first = app.world().resource::<SequenceVoices>().voices(p)[0].1;
            assert_eq!(first.volume_db, -3.0);
            app.update(); // t = 1.5
            assert_eq!(counts(&app), (1, 1, 0), "the same voice keeps playing");
            app.update(); // t = 2.0: fading out
            let fading = app.world().resource::<SequenceVoices>().voices(p)[0].1;
            assert_eq!(fading.id, first.id);
            assert!((fading.volume_db - (-3.0 + fade_db(1.0))).abs() < 1.0e-4);
            app.update(); // t = 2.5
            assert!(app.world().resource::<SequenceVoices>().voices(p)[0].1.volume_db < -8.0);
            // A seek restarts the voice at its new offset.
            app.world_mut()
                .get_mut::<SequencePlayer>(p)
                .expect("player")
                .seek(1.25);
            app.update(); // seek to 1.25, then +0.5 = 1.75
            assert_eq!(counts(&app), (1, 2, 1), "restarted");
            app.update(); // 2.25
            app.update(); // 2.75
            app.update(); // 3.25: past the end
            assert_eq!(counts(&app), (0, 2, 2), "the section ended its voice");
            // Pausing inside a section stops it, playing again restarts it.
            app.world_mut()
                .get_mut::<SequencePlayer>(p)
                .expect("player")
                .seek(1.0);
            app.update();
            assert_eq!(counts(&app).0, 1);
            app.world_mut()
                .get_mut::<SequencePlayer>(p)
                .expect("player")
                .pause();
            app.update();
            assert_eq!(counts(&app).0, 0, "paused players are silent");
            app.world_mut()
                .get_mut::<SequencePlayer>(p)
                .expect("player")
                .play();
            app.update();
            assert_eq!(counts(&app).0, 1);
            // Despawning the player stops its voices.
            let before = counts(&app).2;
            app.world_mut().despawn(p);
            app.update();
            let voices = app.world().resource::<SequenceVoices>();
            assert!(voices.is_empty());
            assert_eq!(voices.stopped(), before + 1);
        }

        /// A spatial section on `hero` (v3, ADR-2405): its voice's gain and pan follow the bound
        /// entity against the listener, as `regia_audio`'s emitter spatialization computes them.
        #[test]
        fn a_spatial_section_follows_the_bound_entity_with_the_listener_math() {
            use super::super::super::asset::{AudioSpatial, SoundRolloff, SoundSource};
            use super::super::super::audio::spatial_frame;
            use regia_audio::ListenerFrame;
            let mut app = audio_app();
            let listener = ListenerFrame::looking_at(Vec3::ZERO, Vec3::NEG_Z);
            app.world_mut().resource_mut::<Audio>().set_listener(listener);
            let hero = app
                .world_mut()
                .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
                .id();
            let mut on_hero = AudioSection::new("theme", 0.0, 3.0);
            on_hero.volume_db = -3.0;
            let mut placement = AudioSpatial::new(SoundSource::Binding("hero".into()));
            placement.max_distance = 40.0;
            on_hero.spatial = Some(placement.clone());
            let mut fixed = AudioSection::new("theme", 0.0, 3.0);
            let mut bell = AudioSpatial::new(SoundSource::Position([0.0, 0.0, -20.0]));
            bell.rolloff = SoundRolloff::Linear;
            bell.min_distance = 2.0;
            fixed.spatial = Some(bell.clone());
            let seq = sequence(
                vec![track("steps", "", Channel::Audio(vec![on_hero, fixed]))],
                vec![marker("end", 4.0)],
            );
            let mut player = SequencePlayer::new(seq);
            player.play();
            let p = app
                .world_mut()
                .spawn((
                    player,
                    SequenceBindings::default().with("hero", Binding::Entity(hero)),
                ))
                .id();
            let voices = |app: &App| app.world().resource::<SequenceVoices>().voices(p);
            let expect = |placement: &AudioSpatial, at: Vec3| {
                spatial_frame(placement, at, Quat::IDENTITY, &[listener])
            };
            app.update(); // t = 0.5
            let v = voices(&app);
            assert_eq!(v.len(), 2, "both sections play");
            let right = expect(&placement, Vec3::new(10.0, 0.0, 0.0));
            assert!(right.pan > 0.5, "hero is to the right: {}", right.pan);
            assert!(
                (right.gain - 0.1).abs() < 1.0e-5,
                "inverse rolloff 1 m / 10 m: {}",
                right.gain
            );
            assert_eq!(v[0].1.pan, right.pan);
            assert_eq!(v[0].1.spatial_gain, right.gain);
            assert!((v[0].1.volume_db - (-3.0 + regia_audio::gain_to_db(right.gain))).abs() < 1.0e-4);
            assert_eq!(v[0].1.position, Some([10.0, 0.0, 0.0]));
            // The fixed bell is straight ahead: centered, linear rolloff 2 m..50 m at 20 m.
            let ahead = expect(&bell, Vec3::new(0.0, 0.0, -20.0));
            assert!(ahead.pan.abs() < 1.0e-5);
            assert!(
                (ahead.gain - (1.0 - 18.0 / 48.0)).abs() < 1.0e-5,
                "{}",
                ahead.gain
            );
            assert_eq!((v[1].1.pan, v[1].1.spatial_gain), (ahead.pan, ahead.gain));
            let first = v[0].1.id;
            // Move hero to the left and closer: the same voice follows.
            app.world_mut()
                .get_mut::<Transform>(hero)
                .expect("hero")
                .translation = Vec3::new(-4.0, 0.0, 0.0);
            app.update(); // t = 1.0
            let v = voices(&app);
            let left = expect(&placement, Vec3::new(-4.0, 0.0, 0.0));
            assert!(left.pan < -0.5, "hero is to the left: {}", left.pan);
            assert!((left.gain - 0.25).abs() < 1.0e-5);
            assert_eq!(v[0].1.id, first, "moving the emitter does not restart the voice");
            assert_eq!((v[0].1.pan, v[0].1.spatial_gain), (left.pan, left.gain));
            assert_eq!(v[0].1.position, Some([-4.0, 0.0, 0.0]));
            // Beyond max_distance it is silent but keeps its voice.
            app.world_mut()
                .get_mut::<Transform>(hero)
                .expect("hero")
                .translation = Vec3::new(0.0, 0.0, 60.0);
            app.update();
            let v = voices(&app);
            assert_eq!(v[0].1.spatial_gain, 0.0);
            assert_eq!(v[0].1.volume_db, -3.0 + regia_audio::gain_to_db(0.0));
            assert_eq!(v[0].1.id, first);
        }

        /// The voice time scale (v3, ADR-2405): varispeed, so a section at 2 plays its sound twice
        /// as fast (the sound position) and at pitch ratio 2.
        #[test]
        fn a_voice_time_scale_moves_the_sound_position_and_pitch_by_varispeed() {
            let mut scaled = AudioSection::new("theme", 0.0, 3.0);
            scaled.time_scale = 2.0;
            scaled.offset = 0.25;
            let seq = sequence(vec![track("music", "", Channel::Audio(vec![scaled]))], vec![]);
            let frame = seq.sample(1.0);
            assert_eq!(frame.sounds[0].position, 0.25 + 2.0);
            assert_eq!(frame.sounds[0].rate, 2.0);
            // Under a sub-sequence at speed 1.5 the time scale is 3.
            let mut library = SequenceLibrary::default();
            library.insert("inner", seq);
            let mut sub_at = SubSection::new("inner", 0.0, 4.0);
            sub_at.speed = 1.5;
            let root = sequence(vec![track("fx", "", Channel::Sub(vec![sub_at]))], vec![]);
            let frame = root.sample_with(0.5, &library);
            assert_eq!(frame.sounds[0].rate, 3.0);
            assert_eq!(frame.sounds[0].position, 0.25 + 0.75 * 2.0);
            // The player starts the voice at that rate.
            let mut app = audio_app();
            app.world_mut().insert_resource(library.clone());
            let mut player = SequencePlayer::new(root);
            player.play();
            let p = app.world_mut().spawn(player).id();
            app.update();
            assert_eq!(voice_ids(&app, p).len(), 1);
        }

        fn voice_ids(app: &App, player: Entity) -> Vec<SoundInstanceId> {
            let voices = app.world().resource::<SequenceVoices>().voices(player);
            voices.into_iter().map(|(_, v)| v.id).collect()
        }

        #[test]
        fn an_edit_of_a_running_section_restarts_its_voice_and_other_edits_do_not() {
            let mut app = audio_app();
            app.world_mut()
                .resource_mut::<SoundAssets>()
                .add("other", tone(4.0));
            let seq = sequence(vec![music("theme", 0.0, 3.0)], vec![marker("m", 4.0)]);
            let mut player = SequencePlayer::new(seq.clone());
            player.play();
            let p = app.world_mut().spawn(player).id();
            app.update();
            let first = voice_ids(&app, p);
            assert_eq!(first.len(), 1);
            // An edit that leaves the sound alone (the marker moved).
            let mut moved = seq.clone();
            moved.markers[0].time = 3.5;
            let replace = |app: &mut App, sequence: Sequence| {
                app.world_mut()
                    .get_mut::<SequencePlayer>(p)
                    .expect("player")
                    .replace_sequence(sequence);
                app.update();
            };
            replace(&mut app, moved.clone());
            assert_eq!(voice_ids(&app, p), first, "the voice plays on");
            // Another sound on the same section.
            let mut swapped = moved.clone();
            let Channel::Audio(sections) = &mut swapped.tracks[0].keys else {
                panic!("audio")
            };
            sections[0].sound = "other".into();
            replace(&mut app, swapped.clone());
            let second = voice_ids(&app, p);
            assert_ne!(second, first, "the old sound stopped");
            // The same sound retimed.
            let Channel::Audio(sections) = &mut swapped.tracks[0].keys else {
                panic!("audio")
            };
            sections[0].offset = 0.75;
            replace(&mut app, swapped);
            assert_ne!(voice_ids(&app, p), second, "a new offset is a new voice");
        }

        #[test]
        fn a_paused_game_clock_and_an_external_jump_stop_or_resync_voices() {
            let mut app = audio_app();
            let seq = sequence(vec![music("theme", 0.0, 3.0)], vec![]);
            let mut player = SequencePlayer::new(seq.clone());
            player.play();
            let p = app.world_mut().spawn(player).id();
            app.update();
            assert_eq!(voice_ids(&app, p).len(), 1);
            app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
            app.update();
            assert!(voice_ids(&app, p).is_empty(), "the game is paused: silence");
            app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(500)));
            app.update();
            assert_eq!(voice_ids(&app, p).len(), 1, "back at the clock's offset");

            let mut player = SequencePlayer::new(seq);
            player.clock = ClockSource::External;
            player.play();
            let e = app.world_mut().spawn(player).id();
            app.update();
            let first = voice_ids(&app, e);
            assert_eq!(first.len(), 1);
            let external = |app: &mut App, t: f64| {
                app.world_mut()
                    .get_mut::<SequencePlayer>(e)
                    .expect("player")
                    .set_external_time(t);
                app.update();
            };
            external(&mut app, 0.5);
            assert_eq!(
                voice_ids(&app, e),
                first,
                "one frame's step: the voice keeps time"
            );
            external(&mut app, 2.0);
            assert_ne!(
                voice_ids(&app, e),
                first,
                "a jump forward lands at the new offset"
            );
        }

        #[test]
        fn hold_audio_keeps_a_running_player_silent() {
            let mut app = audio_app();
            let mut player = SequencePlayer::new(sequence(vec![music("theme", 0.0, 3.0)], vec![]));
            player.hold_audio = true;
            player.play();
            let p = app.world_mut().spawn(player).id();
            app.update();
            assert!(voice_ids(&app, p).is_empty());
            app.world_mut()
                .get_mut::<SequencePlayer>(p)
                .expect("player")
                .hold_audio = false;
            app.update();
            assert_eq!(voice_ids(&app, p).len(), 1);
        }

        #[test]
        fn an_unknown_sound_is_reported_and_no_audio_resource_plays_nothing() {
            let mut app = audio_app();
            let mut player = SequencePlayer::new(sequence(vec![music("nope", 0.0, 2.0)], vec![]));
            player.play();
            let p = app.world_mut().spawn(player).id();
            app.update();
            let player = app.world().get::<SequencePlayer>(p).expect("player");
            let kinds: Vec<&str> = player.issues().map(SequenceIssue::kind).collect();
            assert_eq!(kinds, ["unknown_sound"]);
            assert_eq!(app.world().resource::<SequenceVoices>().started(), 0);

            let mut silent = values_app(Duration::from_millis(500));
            let mut player = SequencePlayer::new(sequence(vec![music("theme", 0.0, 2.0)], vec![]));
            player.play();
            silent.world_mut().spawn(player);
            silent.update();
            assert_eq!(silent.world().resource::<SequenceVoices>().started(), 0);
        }

        #[test]
        fn markers_of_sub_sequences_replay_identically_at_30_and_60_hz() {
            let run = |hz: u32| {
                let mut library = SequenceLibrary::default();
                library.insert("fx", sequence(vec![], vec![marker("a", 0.25), marker("b", 0.5)]));
                let mut parent = sequence(
                    vec![track(
                        "fx",
                        "",
                        Channel::Sub(vec![sub("fx", 0.5, 1.5), sub("fx", 1.0, 2.0)]),
                    )],
                    vec![marker("m", 1.0)],
                );
                parent.tracks[0].name = "fx".into();
                let mut player = SequencePlayer::new(parent);
                player.set_library(&library);
                player.end = EndPolicy::Loop;
                player.play();
                let mut fired = Vec::new();
                for _ in 0..(hz * 5) {
                    player.step(Duration::from_nanos(1_000_000_000 / u64::from(hz)));
                    fired.extend(
                        player
                            .take_fired()
                            .into_iter()
                            .map(|m| (m.name, seconds_to_nanos(m.time), m.cycle)),
                    );
                }
                fired
            };
            let a = run(30);
            assert_eq!(a, run(60));
            assert!(
                a.iter()
                    .any(|(n, t, _)| n == "fx/a" && *t == seconds_to_nanos(0.75))
            );
            assert_eq!(
                a.iter().filter(|(n, _, c)| n == "fx/b" && *c == 0).count(),
                2,
                "two sections"
            );
        }

        /// Two clips on a 64-joint rig reuse the pose buffers on the second frame.
        #[test]
        fn clip_blend_reuses_pose_buffers_across_frames() {
            const JOINTS: usize = 64;
            let mut app = values_app(Duration::from_millis(100));
            let (root, joints) = {
                let names: Vec<String> = (0..JOINTS).map(|i| format!("j{i}")).collect();
                let parents: Vec<Option<u32>> =
                    (0..JOINTS).map(|i| i.checked_sub(1).map(|p| p as u32)).collect();
                let bind = (0..JOINTS)
                    .map(|_| translated(Vec3::new(0.0, 0.1, 0.0)))
                    .collect();
                let skeleton = Arc::new(Skeleton::from_local_bind(names, parents, bind).expect("skeleton"));
                let root = app.world_mut().spawn(Transform::default()).id();
                let binding = spawn_skeleton(app.world_mut(), skeleton);
                let joints = binding.joints.clone();
                app.world_mut().entity_mut(root).insert(binding);
                (root, joints)
            };
            let clip_at = |name: &str, x: f32| {
                let mut clip = AnimationClip::empty(name, 1.0);
                clip.tracks.push(JointTrack {
                    joint: 7,
                    translations: TrackCurve::Static(Vec3::new(x, 0.1, 0.0)),
                    rotations: TrackCurve::Static(Quat::IDENTITY),
                    scales: TrackCurve::Static(Vec3::ONE),
                });
                clip
            };
            app.world_mut()
                .resource_mut::<SequenceClips>()
                .insert("a", clip_at("a", 2.0));
            app.world_mut()
                .resource_mut::<SequenceClips>()
                .insert("b", clip_at("b", 6.0));
            let seq = sequence(
                vec![track(
                    "body",
                    "hero",
                    Channel::Clip(vec![
                        section("a", 0.0, 2.0, 0.0, 0.0),
                        section("b", 0.0, 2.0, 0.0, 0.0),
                    ]),
                )],
                vec![],
            );
            let mut player = SequencePlayer::new(seq);
            player.play();
            app.world_mut().spawn((
                player,
                SequenceBindings::default().with("hero", Binding::Entity(root)),
            ));
            app.update();
            let first = joint_x(&app, joints[7]);
            hot_counts::reset();
            app.update();
            let second = joint_x(&app, joints[7]);
            assert!((first - 4.0).abs() < 1.0e-4, "half of 2 and 6 is 4, got {first}");
            assert!(
                (second - first).abs() < 1.0e-4,
                "the second frame matches the first blend ({first} then {second})"
            );
            assert!(
                hot_counts::pose() < 1,
                "the second frame allocated {} pose buffers",
                hot_counts::pose()
            );
        }

        /// Apply and audio share the sample advance already took.
        #[test]
        fn a_playing_player_samples_once_per_frame() {
            let mut app = audio_app();
            let mut player =
                SequencePlayer::new(sequence(vec![music("theme", 0.0, 4.0)], vec![marker("end", 4.0)]));
            player.play();
            app.world_mut().spawn(player);
            crate::sequencer::sample::sample_counts::reset();
            app.update();
            assert_eq!(
                crate::sequencer::sample::sample_counts::samples(),
                1,
                "a playing frame sampled the tree {} times",
                crate::sequencer::sample::sample_counts::samples()
            );
        }
    }
}

/// Event tracks (`.seq.ron` v6, ADR-8105): keys fire like markers.
#[cfg(test)]
mod event_tests {
    use super::super::asset::fixtures::{float_track, sequence};
    use super::super::asset::{Channel, EventKey, Track};
    use super::*;

    fn event_track(name: &str, keys: Vec<EventKey>) -> Track {
        Track {
            name: name.into(),
            target: "door".into(),
            property: String::new(),
            keys: Channel::Event(keys),
            enabled: true,
            locked: false,
        }
    }

    fn key(time: f32, event: &str, payload: &str) -> EventKey {
        EventKey {
            time,
            event: event.into(),
            payload: payload.into(),
        }
    }

    fn events(fired: &[FiredEvent]) -> Vec<(String, String)> {
        fired
            .iter()
            .map(|e| (e.event.clone(), e.payload.clone()))
            .collect()
    }

    fn player(end: EndPolicy) -> SequencePlayer {
        let mut player = SequencePlayer::new(sequence(
            vec![
                float_track("fade", "ui", "opacity", 4.0),
                event_track(
                    "cues",
                    vec![
                        key(1.0, "open", "fast"),
                        key(1.0, "chime", ""),
                        key(3.0, "close", ""),
                    ],
                ),
                event_track("more", vec![key(1.0, "light", "on")]),
            ],
            Vec::new(),
        ));
        player.end = end;
        player.play();
        player
    }

    #[test]
    fn an_event_key_fires_once_in_forward_play() {
        let mut p = player(EndPolicy::Hold);
        // Small steps across 1.0: each key fires exactly once, on the step that crosses it, in
        // time, then track, then file order.
        let mut fired = Vec::new();
        for _ in 0..40 {
            p.step(Duration::from_millis(50));
            fired.extend(p.take_fired_events());
        }
        assert_eq!(p.time(), 2.0);
        assert_eq!(
            events(&fired),
            [
                ("open".into(), "fast".into()),
                ("chime".into(), String::new()),
                ("light".into(), "on".into())
            ]
        );
        assert!(fired.iter().all(|e| !e.skipped && e.cycle == 0 && e.time == 1.0));
        assert_eq!(fired[0].track, "cues");
        assert_eq!(fired[0].target, "door");
        assert_eq!(fired[2].track, "more");
        p.step(Duration::ZERO);
        assert!(p.take_fired_events().is_empty(), "a zero step fires nothing more");
        // A seek (and a scrub, which is a seek) back over the keys fires nothing; playing forward
        // again crosses them again.
        p.seek(0.5);
        assert!(p.take_fired_events().is_empty(), "a seek fires nothing");
        p.step(Duration::from_millis(600));
        assert_eq!(
            p.take_fired_events().len(),
            3,
            "forward play from the seek crosses them again"
        );
        // Reaching the end under Hold fires the last key once.
        p.step(Duration::from_secs(10));
        assert_eq!(events(&p.take_fired_events()), [("close".into(), String::new())]);
        p.step(Duration::from_secs(1));
        assert!(p.take_fired_events().is_empty(), "a held end fires nothing more");
        assert_eq!(p.events_fired(), 7);
    }

    #[test]
    fn events_follow_loops_skips_mutes_and_never_fire_on_reverse_or_seek() {
        // A loop fires each key once per pass, the keys before the wrap first.
        let mut p = player(EndPolicy::Loop);
        p.step(Duration::from_millis(3500));
        p.take_fired_events();
        p.step(Duration::from_millis(1000));
        let fired = p.take_fired_events();
        assert!(fired.is_empty(), "3.5 -> 0.5 crosses no key: {fired:?}");
        p.step(Duration::from_millis(3000));
        let fired = p.take_fired_events();
        assert_eq!(fired.len(), 4, "{fired:?}");
        assert!(fired.iter().all(|e| e.cycle == 1));
        // A skip delivers what it jumps over with `skipped`.
        let mut p = player(EndPolicy::Hold);
        p.skip_to(3.5);
        let fired = p.take_fired_events();
        assert_eq!(fired.len(), 4);
        assert!(fired.iter().all(|e| e.skipped));
        // A muted (disabled) event track fires nothing.
        let mut sequence = p.sequence().clone();
        sequence.tracks[1].enabled = false;
        let mut p = SequencePlayer::new(sequence);
        p.play();
        p.step(Duration::from_secs(4));
        assert_eq!(events(&p.take_fired_events()), [("light".into(), "on".into())]);
        // An external clock moved backward is a seek: nothing fires; forward fires.
        let mut p = player(EndPolicy::Hold);
        p.clock = ClockSource::External;
        p.set_external_time(2.0);
        assert_eq!(p.take_fired_events().len(), 3);
        p.set_external_time(0.5);
        assert!(p.take_fired_events().is_empty(), "reverse fires nothing");
        p.set_external_time(1.5);
        assert_eq!(p.take_fired_events().len(), 3);
    }

    #[derive(Resource, Default)]
    struct Seen(Vec<SequenceEventFired>);

    fn collect(mut reader: MessageReader<SequenceEventFired>, mut seen: ResMut<Seen>) {
        seen.0.extend(reader.read().cloned());
    }

    #[test]
    fn the_plugin_sends_sequence_event_fired_messages() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(250)))
            .init_resource::<Seen>()
            .add_systems(Update, collect);
        let entity = app.world_mut().spawn(player(EndPolicy::Hold)).id();
        for _ in 0..8 {
            app.update();
        }
        let seen = &app.world().resource::<Seen>().0;
        let names: Vec<&str> = seen.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(names, ["open", "chime", "light"], "{seen:?}");
        assert!(seen.iter().all(|e| e.player == entity && !e.skipped));
        assert_eq!(seen[0].payload, "fast");
        assert_eq!(seen[0].target, "door");
    }
}
