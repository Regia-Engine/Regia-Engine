// SPDX-License-Identifier: MPL-2.0
//! The cinematics sequencer (`.seq.ron`): one master clock of named tracks and markers
//! (`docs/formats/seq-ron.md`, ADR-1507), and the player that runs it (ADR-1614).
//!
//! - [`asset`]: the `.seq.ron` format ([`Sequence`]), versioned and validated.
//! - [`sample`]: [`Sequence::sample`], the function from a time to the value of every
//!   enabled track, and [`Sequence::markers_between`] for deterministic marker dispatch. No
//!   clocks. A duration cache remembers the length and does not change the sampled frame.
//! - [`player`]: [`SequencePlayer`] (one entity per playing sequence, optionally with a
//!   `.shots.ron` shot list on the same clock), [`SequenceBindings`], the [`PropertyAppliers`]
//!   registry, the [`MarkerReached`] and [`SequenceFinished`] messages, and [`SequencerPlugin`].
//! - [`takeover`]: the cinematic camera takeover: a player's shots blend a camera in from the
//!   gameplay pose and back out, the most recent start wins, and director shakes play on top.
//!
//! The data model is plain keyed values (seconds on a master clock, and whatever unit each
//! property means), so a hand-written file, an agent and the future timeline panel all edit the
//! same thing. It is target-agnostic: a track names a `target` binding and a `property` path,
//! and animates one channel (a float, a vec3 or a bool). Camera moves stay in `.shots.ron`
//! (ADR-268) — per-shot clocks, cuts, fades and depth of field — and the two compose in the
//! player. The agent surface is `sequence.player` (book `agents/sequence-player.md`).
//!
//! - [`audio`]: the voices of audio tracks ([`SequenceVoices`]).
//!
//! `.seq.ron` v2 (ADR-1880) adds Bezier keys with tangents and the clip (with blending), audio
//! and sub-sequence tracks. The editor's sequence curve panel edits the keyed tracks.
//!
//! - [`sections`]: the edits of clip, audio and sub-sequence sections (move, trim, speed, offset,
//!   ramps, gain, add, duplicate, remove) and their row layout, for the editor's section strip
//!   (ADR-2135).
//!
//! `.seq.ron` v3 (ADR-2405) adds spatial sound sections, a time scale on sound sections and
//! looping sub-sequence sections.
//!
//! `.seq.ron` v4 (ADR-2862) adds video sections.
//!
//! `.seq.ron` v5 adds spawnable bindings and `restore_on_stop` (ADR-2948), and Doppler, occlusion
//! and a pitch-preserving time scale on sound sections (ADR-3197).
//!
//! - [`rename`]: renaming a binding everywhere a sequence refers to it, as one edit.
//!
//! - [`marker_sheet`]: a CSV exchange of the marker list (ADR-3675). Applying a sheet replaces
//!   `markers` and leaves every channel as it is. `.seq.ron` does not change version.
//!
//! - [`recorder`]: the take recorder (ADR-2637): play the game and capture transforms, cameras,
//!   animation clips, properties and audio cues into a `.seq.ron` v3 take with reduced keys.
//!
//! - [`spawnables`]: bindings the player spawns from a prefab for a time range, with a pre-roll
//!   that loads the prefabs first (`.seq.ron` v5, ADR-2948).
//! - [`cutscene`]: the cutscene flow around a player: input lock, letterbox, hidden HUD layers,
//!   skip (hold, fade) and hand-back, and multiplayer starts at a tick (ADR-2948).
//!
//! - [`video`]: video sections on a player's clock, slaved to the sequence time with drift
//!   correction, frame-exact in render-out ([`SequenceVideos`], ADR-2862).

pub mod asset;
#[cfg(feature = "client")]
pub mod audio;
#[cfg(feature = "client")]
pub mod cutscene;
pub mod edit;
pub mod marker_sheet;
#[cfg(feature = "client")]
pub mod player;
#[cfg(feature = "client")]
pub mod recorder;
pub mod rename;
pub mod sample;
pub mod sections;
#[cfg(feature = "client")]
pub mod spawnables;
#[cfg(feature = "client")]
pub mod stream_prefetch;
#[cfg(feature = "client")]
pub mod subtitles;
#[cfg(feature = "client")]
pub mod takeover;
#[cfg(feature = "client")]
pub mod video;

pub use asset::{
    AudioSection, AudioSpatial, BoolKey, Channel, ClipSection, DEFAULT_WEIGHT, DOPPLER_RATIO_RANGE, Ease,
    EventKey, MAX_DOPPLER_FACTOR, MAX_LOOP_COUNT, MAX_LOOP_DEPTH, MAX_SECTIONS, MAX_SPAWNABLES,
    MAX_SUB_DEPTH, MAX_TREE_SECTIONS, MAX_TREE_SEQUENCES, MIN_LOOP_LENGTH, Marker, PRESERVE_PITCH_RANGE,
    SEQUENCE_FORMAT_VERSION, ScalarKey, Sequence, SequenceError, SoundRolloff, SoundSource, Spawnable,
    SubRepeat, SubSection, Tangent, TimeScaleMode, Track, Vec3Key, VideoFit, VideoScreenSlot, VideoSection,
    VideoTarget, sequence_migration_sources, sequence_schema_fingerprint,
};
#[cfg(feature = "client")]
pub use audio::{
    LISTENER_CUT_SPEED, MOTION_PROBE_SECS, OCCLUSION_OWNER, STRETCH_CACHE_BYTES, STRETCH_INLINE_SECS,
    STRETCH_LOOKAHEAD_SECS, SequenceStretches, SequenceVoice, SequenceVoices, StretchStats, fade_db,
};
#[cfg(feature = "client")]
pub use cutscene::{
    CUTSCENE_CONTEXT, CutsceneCommand, CutsceneEnded, CutsceneError, CutscenePhase, CutsceneSettings,
    CutsceneState, Cutscenes, HudLayer, LETTERBOX_SORT_ORDER, NetCutscenes, SKIP_ACTION, SkipSettings,
    cutscene_state, skip_cutscene, start_cutscene, stop_cutscene,
};
#[cfg(feature = "client")]
pub use player::{
    ApplyError, Binding, ClockSource, EndPolicy, FiredEvent, FiredMarker, MAX_REPLAYED_CYCLES, MarkerReached,
    PlaybackStatus, PropertyApplier, PropertyAppliers, SequenceBindings, SequenceClips, SequenceEventFired,
    SequenceFinished, SequenceIssue, SequencePlayer, SequenceRestore, SequenceStarts, SequencerPlugin,
    SequencerSet, TakeoverSettings, blend_clips_into, nanos_to_seconds, seconds_to_nanos, wrap_clip_time,
};
#[cfg(feature = "client")]
pub use recorder::{
    ErrorMetric, MAX_TAKE_BINDINGS, MAX_TAKE_CUES, MAX_TAKE_NUMBER, MAX_TAKE_SAMPLES, PropertyReader,
    PropertyReaders, RecordedTake, TakeBinding, TakeBindingState, TakeChannel, TakeConfig, TakeCue,
    TakeError, TakeFiles, TakeRecorder, TakeStatus, TakeTarget, TakeTolerance, link_take, reduce_samples,
    save_take, take_path,
};
pub use rename::{BindingRename, rename_binding};
pub use sample::{
    ChannelValue, ClipSample, ClipWeight, FlatMarker, MAX_FLAT_MARKERS, SequenceFrame, SequenceLibrary,
    SoundSample, SubPass, TrackSample, VideoSample,
};
#[cfg(feature = "client")]
pub use spawnables::{LiveActor, SequenceSpawnables, SpawnStats, SpawnableChanged, SpawnedBySequence};
#[cfg(feature = "client")]
pub use stream_prefetch::{SequencePrefetchSource, StreamPrefetch};
#[cfg(feature = "client")]
pub use subtitles::{
    ActiveSubtitle, NO_SUBTITLES_SUFFIX, SequenceSubtitleSettings, SequenceSubtitles, active_cue,
    line_for_sound,
};
#[cfg(feature = "client")]
pub use takeover::{CameraTakeover, CinematicFade, TakeoverPose, blend_curve};
#[cfg(feature = "client")]
pub use video::{SequenceVideos, VideoSectionStatus};
