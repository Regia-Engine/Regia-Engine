// SPDX-License-Identifier: MPL-2.0
//! The player state machine and clock (specs/50 § 2.2, § 2.5, § 2.6, § 7), independent of the ECS so
//! every transition is unit-testable. [`PlayerCore`] owns a [`FrameFeed`], selects the presented picture
//! by the integer presentation rule, and queues [`VideoEvent`]s in emission order.
//!
//! Media time is kept on an unbounded "pass" timeline: pass `k` of a looping clip covers
//! `k * clip ..< (k + 1) * clip`, and pictures of a later pass carry `pts + k * clip`. The feed rewinds
//! as soon as a pass's last packet is read, so the next pass's first pictures are queued before the
//! wrap (seamless in `Lockstep`, accurate to one frame period in real time).

use std::collections::VecDeque;

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::codec::DecodedPicture;
use crate::container::{Marker, MediaInfo};
use crate::error::MediaError;
use crate::session::{FeedPoll, FeedStats, FrameFeed};

/// Player machine state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PlayState {
    /// No session.
    #[default]
    Closed,
    /// Waiting for the first picture.
    Opening,
    /// First picture shown, not playing.
    Ready,
    /// Clock running.
    Playing,
    /// Clock frozen.
    Paused,
    /// Catching up to a seek target.
    Seeking,
    /// Starved of pictures; clock frozen.
    Buffering,
    /// Last picture shown, loop off.
    Ended,
    /// Unrecoverable error; reopen to continue.
    Failed,
}

impl PlayState {
    /// Lowercase name (agent surface).
    pub fn name(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Opening => "opening",
            Self::Ready => "ready",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Seeking => "seeking",
            Self::Buffering => "buffering",
            Self::Ended => "ended",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for PlayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Loop policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Reflect)]
pub enum LoopMode {
    /// Play once.
    #[default]
    Off,
    /// Loop forever.
    Loop,
    /// Play this many times in total.
    Count(u32),
}

impl LoopMode {
    /// Total passes allowed (`u32::MAX` for forever).
    pub fn passes(self) -> u32 {
        match self {
            Self::Off => 1,
            Self::Loop => u32::MAX,
            Self::Count(n) => n.max(1),
        }
    }
}

/// What drives media time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Reflect)]
pub enum ClockMode {
    /// Frames consumed by the audio output.
    Audio,
    /// Game time delta times rate.
    #[default]
    Game,
    /// A time written each frame by the sequencer or a script.
    External,
    /// One fixed step per rendered frame (render-out, tests).
    Lockstep,
}

impl ClockMode {
    /// Parses a lowercase or capitalized mode name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "audio" => Self::Audio,
            "game" => Self::Game,
            "external" => Self::External,
            "lockstep" => Self::Lockstep,
            _ => return None,
        })
    }

    /// Lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Game => "game",
            Self::External => "external",
            Self::Lockstep => "lockstep",
        }
    }
}

/// Seek precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Reflect)]
pub enum SeekMode {
    /// Land on the first picture at or after the target.
    #[default]
    Accurate,
    /// Land on the cue point at or before the target.
    Keyframe,
}

/// A notification (specs/50 § 3.4), in emission order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VideoEvent {
    /// The first picture is ready.
    Opened,
    /// Opening failed.
    OpenFailed {
        /// Protocol code.
        code: String,
    },
    /// The session closed.
    Closed,
    /// Playback ended.
    Ended {
        /// Ended by a skip.
        skipped: bool,
    },
    /// Playback wrapped.
    Looped {
        /// Wraps so far.
        count: u32,
    },
    /// A playlist finished.
    ListEnded,
    /// Starved of pictures.
    BufferingStarted,
    /// Pictures arrived again.
    BufferingDone,
    /// A seek landed.
    Seeked {
        /// Landed media time in seconds.
        time: f64,
    },
    /// Paused.
    Paused,
    /// Resumed from pause.
    Resumed,
    /// The audio track changed.
    TracksChanged,
    /// Playback crossed a chapter marker.
    Marker {
        /// Chapter title.
        name: String,
    },
    /// The session failed.
    Failed {
        /// Protocol code.
        code: String,
    },
    /// Playback entered a text cue (specs/50 § 2.8, ADR-2410).
    TextCue {
        /// Index of the text track in `MediaInfo::tracks`.
        track: u32,
        /// Cue start in seconds.
        start: f64,
        /// Cue end in seconds, when known (block duration, next cue or clip end).
        end: Option<f64>,
        /// The raw UTF-8 text.
        text: String,
    },
    /// Playback left a text cue.
    TextCueEnd {
        /// Index of the text track.
        track: u32,
        /// Start of the cue left, in seconds.
        start: f64,
    },
}

impl VideoEvent {
    /// Lowercase kind name (agent surface).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::OpenFailed { .. } => "open_failed",
            Self::Closed => "closed",
            Self::Ended { .. } => "ended",
            Self::Looped { .. } => "looped",
            Self::ListEnded => "list_ended",
            Self::BufferingStarted => "buffering_started",
            Self::BufferingDone => "buffering_done",
            Self::Seeked { .. } => "seeked",
            Self::Paused => "paused",
            Self::Resumed => "resumed",
            Self::TracksChanged => "tracks_changed",
            Self::Marker { .. } => "marker",
            Self::Failed { .. } => "failed",
            Self::TextCue { .. } => "text_cue",
            Self::TextCueEnd { .. } => "text_cue_end",
        }
    }
}

/// Player counters (specs/50 § 3.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct VideoStats {
    /// Pictures presented.
    pub frames_presented: u64,
    /// Due pictures skipped because a newer one was also due (never in `Lockstep`).
    pub frames_dropped: u64,
    /// Ticks that kept the previous picture beyond one frame period.
    pub frames_repeated: u64,
    /// Packets rejected by the decoder or the caps.
    pub frames_corrupt: u64,
    /// Packets read from the container.
    pub packets_read: u64,
    /// Pictures waiting.
    pub queue_depth: u32,
    /// Moving average of decode time per packet.
    pub decode_ms_ema: f64,
    /// Audio underruns reported by the mixer.
    pub underruns: u64,
    /// Decoding stayed slower than the frame rate for 60 pictures in a row at some point this session.
    pub decode_too_slow: bool,
}

/// One tick's clock input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockInput {
    /// `Lockstep`: one fixed step of `dt_ns`.
    Step {
        /// Fixed step.
        dt_ns: u64,
    },
    /// `Game`: elapsed game time.
    Elapsed {
        /// Delta.
        dt_ns: u64,
    },
    /// `External`: the media time to show.
    Time {
        /// Media time.
        t_ns: u64,
    },
    /// `Audio`: the audio clock's total frames consumed.
    Audio {
        /// Frames consumed since the feed was created.
        frames: u64,
        /// Mix rate.
        sample_rate: u32,
    },
}

/// Rates are kept in thousandths so every clock product is integer arithmetic.
fn rate_milli(rate: f32) -> Result<u32, MediaError> {
    if !rate.is_finite() || !(0.25..=4.0).contains(&rate) {
        return Err(MediaError::RateUnsupported(rate.to_string()));
    }
    Ok((rate * 1000.0).round() as u32)
}

/// Audio frames to take on lockstep step `step` so the total after `n` steps is exactly
/// `floor(n * sample_rate * dt * rate)` (integer remainder carried, specs/50 § 7).
pub fn lockstep_audio_frames(sample_rate: u32, dt_ns: u64, rate_milli: u32, step: u64) -> u64 {
    let per = |n: u64| -> u128 {
        u128::from(n) * u128::from(sample_rate) * u128::from(dt_ns) * u128::from(rate_milli)
            / 1_000_000_000_000
    };
    (per(step + 1) - per(step)) as u64
}

/// Most `VideoEvent::Looped` events one tick reports. A tick that crosses more wraps (a very short
/// clip, a long tick) reports only the last ones; `count` still carries the running total and
/// [`PlayerCore::loops_done`] counts every wrap (ADR-7563).
pub const MAX_LOOPED_EVENTS_PER_TICK: usize = 4;

/// Ticks a resync after a decode pause waits for the picture due at the clock before it presents
/// the newest one that arrived (a decoder slower than real time would otherwise never land).
const RESYNC_LANDING_TICKS: u32 = 120;

/// Wraps completed at pass-timeline time `t_ns` over a clip of `clip_ns`, at most `passes - 1`.
fn loops_at(t_ns: i64, clip_ns: i64, passes: u32) -> u32 {
    if t_ns <= 0 {
        return 0;
    }
    let wraps = t_ns / clip_ns.max(1);
    u32::try_from(wraps)
        .unwrap_or(u32::MAX)
        .min(passes.saturating_sub(1))
}

/// Lockstep presentation time of step `n` (integer rule of specs/50 § 7).
pub fn lockstep_time_ns(start_ns: i64, n: u64, dt_ns: u64, rate_milli: u32) -> i64 {
    let advance = u128::from(n) * u128::from(dt_ns) * u128::from(rate_milli) / 1000;
    start_ns.saturating_add(advance.min(i64::MAX as u128) as i64)
}

/// The engine-independent player.
pub struct PlayerCore {
    state: PlayState,
    resume: PlayState,
    feed: Option<Box<dyn FrameFeed>>,
    info: Option<MediaInfo>,
    clip_ns: Option<u64>,
    frame_ns: u64,
    rate_milli: u32,
    loop_mode: LoopMode,
    clock: ClockMode,
    autoplay: bool,
    /// Media time on the pass timeline.
    t_ns: i64,
    loops_done: u32,
    feed_pass: u32,
    feed_done: bool,
    last_feed_pts: i64,
    queue: VecDeque<DecodedPicture>,
    presented: Option<DecodedPicture>,
    presented_abs: i64,
    pending_seek: Option<(u64, SeekMode)>,
    seek_scan: Option<(u64, SeekMode)>,
    lock_base: i64,
    lock_steps: u64,
    audio_anchor: Option<(u64, i64)>,
    starve_ns: u64,
    starve_limit_ns: u64,
    resume_frames: usize,
    events: Vec<VideoEvent>,
    stats: VideoStats,
    last_error: Option<MediaError>,
    ended_fired: bool,
    slow_warning: bool,
    cues: Option<crate::cues::CueTracker>,
    decode_paused: bool,
    clock_frozen: bool,
    /// Held by the gameplay pause (ADR-10105): the playing clock stops; the release re-anchors it.
    game_held: bool,
    resync: bool,
    /// After a resync's accurate seek: ticks spent waiting for the picture due at the clock.
    resync_landing: Option<u32>,
    quiet_drops: bool,
    half_rate: bool,
    half_rate_skip: bool,
    paused_ticks: u64,
    /// Since [`PlayerCore::reposition`]: its target, until a picture lands.
    landing: Option<i64>,
}

impl std::fmt::Debug for PlayerCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayerCore")
            .field("state", &self.state)
            .field("t_ns", &self.t_ns)
            .finish_non_exhaustive()
    }
}

impl Default for PlayerCore {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayerCore {
    /// A closed player.
    ///
    /// ```
    /// use regia_media::{LoopMode, PlayState, PlayerCore};
    ///
    /// let player = PlayerCore::new();
    /// assert_eq!(player.state(), PlayState::Closed);
    /// assert_eq!(player.loop_mode(), LoopMode::Off);
    /// assert_eq!(player.position_ns(), 0);
    /// assert!(player.info().is_none());
    /// ```
    pub fn new() -> Self {
        Self {
            state: PlayState::Closed,
            resume: PlayState::Paused,
            feed: None,
            info: None,
            clip_ns: None,
            frame_ns: 33_333_333,
            rate_milli: 1000,
            loop_mode: LoopMode::Off,
            clock: ClockMode::Game,
            autoplay: false,
            t_ns: 0,
            loops_done: 0,
            feed_pass: 0,
            feed_done: false,
            last_feed_pts: 0,
            queue: VecDeque::new(),
            presented: None,
            presented_abs: 0,
            pending_seek: None,
            seek_scan: None,
            lock_base: 0,
            lock_steps: 0,
            audio_anchor: None,
            game_held: false,
            starve_ns: 0,
            starve_limit_ns: 100_000_000,
            resume_frames: 2,
            events: Vec::new(),
            stats: VideoStats::default(),
            last_error: None,
            ended_fired: false,
            slow_warning: false,
            cues: None,
            decode_paused: false,
            clock_frozen: false,
            resync: false,
            resync_landing: None,
            quiet_drops: false,
            half_rate: false,
            half_rate_skip: false,
            paused_ticks: 0,
            landing: None,
        }
    }

    // ---- accessors ------------------------------------------------------------------------------

    /// Machine state.
    pub fn state(&self) -> PlayState {
        self.state
    }

    /// Position in the current pass, seconds.
    pub fn position(&self) -> f64 {
        self.position_ns() as f64 / 1e9
    }

    /// Position in the current pass, nanoseconds.
    pub fn position_ns(&self) -> i64 {
        let clip = self.clip_ns.unwrap_or(u64::MAX).min(i64::MAX as u64) as i64;
        (self.t_ns - i64::from(self.loops_done).saturating_mul(clip)).max(0)
    }

    /// Clip duration in seconds, if known.
    pub fn duration(&self) -> Option<f64> {
        self.clip_ns.map(|c| c as f64 / 1e9)
    }

    /// Index of the presented picture at the nominal rate.
    pub fn frame_index(&self) -> Option<u64> {
        let p = self.presented.as_ref()?;
        Some(frame_index_at(p.pts_ns, self.frame_ns))
    }

    /// The presented picture.
    pub fn presented(&self) -> Option<&DecodedPicture> {
        self.presented.as_ref()
    }

    /// File information of the open session.
    pub fn info(&self) -> Option<&MediaInfo> {
        self.info.as_ref()
    }

    /// Counters.
    pub fn stats(&self) -> VideoStats {
        let mut s = self.stats;
        s.queue_depth = self.queue.len() as u32;
        s
    }

    /// Ring counters of the open session's decode ring, when it has one (ADR-2044).
    pub fn ring_stats(&self) -> Option<crate::ring::RingStats> {
        self.feed.as_ref().and_then(|f| f.ring())
    }

    /// True once per session, when `decode_too_slow` first turns on (specs/50 § 10).
    pub fn take_slow_warning(&mut self) -> bool {
        std::mem::take(&mut self.slow_warning)
    }

    /// Current rate.
    pub fn rate(&self) -> f32 {
        self.rate_milli as f32 / 1000.0
    }

    /// Loop policy.
    pub fn loop_mode(&self) -> LoopMode {
        self.loop_mode
    }

    /// Clock mode.
    pub fn clock(&self) -> ClockMode {
        self.clock
    }

    /// Wraps so far.
    pub fn loops_done(&self) -> u32 {
        self.loops_done
    }

    /// The last error.
    pub fn last_error(&self) -> Option<&MediaError> {
        self.last_error.as_ref()
    }

    /// Attaches the session's text cues (the decode session fills `store`; ADR-2410).
    pub fn set_cues(&mut self, store: crate::cues::CueStore) {
        self.cues = Some(crate::cues::CueTracker::new(store));
    }

    /// The text cues of the open session, if it has a cue store.
    pub fn cue_store(&self) -> Option<&crate::cues::CueStore> {
        self.cues.as_ref().map(crate::cues::CueTracker::store)
    }

    /// Cues the position is inside now, as (start ns, track index).
    pub fn active_cues(&self) -> &[(i64, u32)] {
        match &self.cues {
            Some(c) => c.active(),
            None => &[],
        }
    }

    /// Holds (`true`) or releases the playing clock for the gameplay pause (`Time::paused`,
    /// ADR-10105). Every clock mode stops while held, the audio and lockstep clocks included (they
    /// would otherwise follow the mixer or the frame count through the pause); the release picks
    /// up where the clock stopped, so the position after the pause is the one before it. Separate
    /// from the transport ([`Self::pause`]) and from [`Self::set_decode_paused`], which the host
    /// owns. The media plugin calls it every frame with `Time::paused`.
    pub fn set_game_held(&mut self, held: bool) {
        if held == self.game_held {
            return;
        }
        self.game_held = held;
        if !held {
            self.lock_base = self.t_ns;
            self.lock_steps = 0;
            self.audio_anchor = None;
        }
    }

    /// Whether the gameplay pause holds the clock ([`Self::set_game_held`]).
    pub fn game_held(&self) -> bool {
        self.game_held
    }

    /// Stops (`true`) or resumes decoding while the clock keeps running, or with `freeze_clock` also
    /// stops the clock (specs/50 § 2.7 `PauseDecode` / `PauseAll`). While paused nothing is pulled,
    /// presented or uploaded; on resume the player re-seeks to the current media time and presents
    /// the picture the presentation rule picks there, so the frame index is the one a never-paused
    /// player shows. `Lockstep` re-seeks blocking, so the frame is right on the resuming tick.
    pub fn set_decode_paused(&mut self, paused: bool, freeze_clock: bool) {
        if paused == self.decode_paused && (!paused || freeze_clock == self.clock_frozen) {
            return;
        }
        if paused {
            self.decode_paused = true;
            self.clock_frozen = freeze_clock;
            return;
        }
        self.decode_paused = false;
        if self.clock_frozen {
            // The frozen clock picks up where it stopped.
            self.lock_base = self.t_ns;
            self.lock_steps = 0;
            self.audio_anchor = None;
        }
        self.clock_frozen = false;
        self.resync = self.presented.is_some();
    }

    /// Whether decoding is paused by visibility or thermal policy.
    pub fn decode_paused(&self) -> bool {
        self.decode_paused
    }

    /// Ticks spent with decoding paused this session.
    pub fn paused_ticks(&self) -> u64 {
        self.paused_ticks
    }

    /// Presents on every other tick only (thermal throttling of non-fullscreen players, specs/50
    /// § 9). Ignored in `Lockstep`, which never drops.
    pub fn set_half_rate(&mut self, on: bool) {
        self.half_rate = on;
        if !on {
            self.half_rate_skip = false;
        }
    }

    /// Whether thermal half-rate presentation is on.
    pub fn half_rate(&self) -> bool {
        self.half_rate
    }

    /// Whether the session is decoding, so it holds a slot of the decode pool (specs/50 § 2.4,
    /// "concurrently decoding players"): opening, playing, seeking or buffering, and not paused by
    /// visibility or thermal policy. Ready, paused, ended, closed and failed players do not decode.
    pub fn holds_decoder(&self) -> bool {
        matches!(
            self.state,
            PlayState::Opening | PlayState::Playing | PlayState::Seeking | PlayState::Buffering
        ) && !self.decode_paused
    }

    /// Takes the queued events.
    pub fn drain_events(&mut self) -> Vec<VideoEvent> {
        std::mem::take(&mut self.events)
    }

    /// One playlist entry failed to open. The player stays in its current state so the next entry
    /// can still open.
    pub fn note_open_failed(&mut self, err: &MediaError) {
        self.last_error = Some(err.clone());
        self.events.push(VideoEvent::OpenFailed {
            code: err.code().to_string(),
        });
    }

    /// The playlist has no further entry. Does not close the session.
    pub fn note_list_ended(&mut self) {
        self.events.push(VideoEvent::ListEnded);
    }

    /// The audio track choice changed on a player that was already open.
    pub fn note_tracks_changed(&mut self) {
        self.events.push(VideoEvent::TracksChanged);
    }

    /// Every entry of a playlist failed to open.
    pub fn fail_playlist(&mut self, err: MediaError) {
        if self.state != PlayState::Closed {
            self.close();
        }
        self.state = PlayState::Failed;
        self.events.push(VideoEvent::Failed {
            code: err.code().to_string(),
        });
        self.last_error = Some(err);
    }

    /// Sets the audio underrun count (the mixer stream's own counter).
    pub fn set_underruns(&mut self, n: u64) {
        self.stats.underruns = n;
    }

    // ---- commands -------------------------------------------------------------------------------

    /// Opens a session. With `Lockstep`, blocks until the first picture (and the `phase_offset` seek)
    /// is done; otherwise the player stays `Opening` until a tick finds the first picture.
    pub fn open(
        &mut self,
        feed: Box<dyn FrameFeed>,
        info: MediaInfo,
        clock: ClockMode,
        loop_mode: LoopMode,
        autoplay: bool,
        phase_offset_s: f64,
    ) {
        if self.state != PlayState::Closed {
            self.close();
        }
        let declared_frame_ns = info
            .video_track()
            .and_then(|t| t.default_duration_ns)
            .filter(|d| *d > 0);
        let frame_ns = declared_frame_ns.unwrap_or(33_333_333);
        // A declared length shorter than the track's own frame period is not the clip's: ignore
        // it and learn the length from the stream's end, as for a file that declares none
        // (deep-zai-media finding 1).
        let clip_ns = info
            .duration_ns
            .filter(|d| *d > 0 && *d >= declared_frame_ns.unwrap_or(0));
        if clip_ns.is_none() && info.duration_ns.is_some_and(|d| d > 0) {
            tracing::warn!(
                target: "regia_media",
                duration_ns = info.duration_ns.unwrap_or(0),
                frame_ns,
                "declared duration shorter than one frame period ignored"
            );
        }
        *self = Self {
            events: std::mem::take(&mut self.events),
            rate_milli: self.rate_milli,
            ..Self::new()
        };
        self.feed = Some(feed);
        self.info = Some(info);
        self.frame_ns = frame_ns;
        self.clip_ns = clip_ns;
        self.clock = clock;
        self.loop_mode = loop_mode;
        self.autoplay = autoplay;
        self.state = PlayState::Opening;
        if phase_offset_s.is_finite() && phase_offset_s > 0.0 {
            self.pending_seek = Some(((phase_offset_s * 1e9) as u64, SeekMode::Accurate));
        }
        if clock == ClockMode::Lockstep {
            self.finish_opening(true);
        }
    }

    /// Records an open failure (the caller could not build a feed).
    pub fn fail_open(&mut self, err: MediaError) {
        if self.state != PlayState::Closed {
            self.close();
        }
        self.state = PlayState::Failed;
        self.events.push(VideoEvent::OpenFailed {
            code: err.code().to_string(),
        });
        self.last_error = Some(err);
    }

    /// `play`.
    pub fn play(&mut self) -> Result<(), MediaError> {
        match self.state {
            PlayState::Opening => {
                self.autoplay = true;
                Ok(())
            }
            PlayState::Ready => {
                self.start_playing();
                Ok(())
            }
            PlayState::Paused => {
                self.start_playing();
                self.events.push(VideoEvent::Resumed);
                Ok(())
            }
            PlayState::Seeking | PlayState::Buffering => {
                self.resume = PlayState::Playing;
                Ok(())
            }
            PlayState::Playing => Ok(()),
            PlayState::Ended => {
                // Equivalent to seek(0) then play.
                self.loops_done = 0;
                self.ended_fired = false;
                self.t_ns = 0;
                self.state = PlayState::Playing;
                self.begin_seek(0, SeekMode::Accurate);
                Ok(())
            }
            PlayState::Closed | PlayState::Failed => Err(MediaError::bad_state("play", self.state)),
        }
    }

    fn start_playing(&mut self) {
        self.state = PlayState::Playing;
        self.lock_base = self.t_ns;
        self.lock_steps = 0;
        self.audio_anchor = None;
        self.starve_ns = 0;
    }

    /// `pause`.
    pub fn pause(&mut self) -> Result<(), MediaError> {
        match self.state {
            PlayState::Playing => {
                self.state = PlayState::Paused;
                self.events.push(VideoEvent::Paused);
                Ok(())
            }
            PlayState::Seeking | PlayState::Buffering => {
                self.resume = PlayState::Paused;
                Ok(())
            }
            PlayState::Paused | PlayState::Ready | PlayState::Opening => {
                self.autoplay = false;
                Ok(())
            }
            _ => Err(MediaError::bad_state("pause", self.state)),
        }
    }

    /// `set_rate`: `0.0` pauses; other rates must be in `0.25..=4.0`.
    pub fn set_rate(&mut self, rate: f32) -> Result<(), MediaError> {
        if rate == 0.0 {
            return self.pause();
        }
        let r = rate_milli(rate)?;
        // Re-anchor the lockstep clock so the new rate applies from here.
        self.lock_base = self.t_ns;
        self.lock_steps = 0;
        self.audio_anchor = None;
        self.rate_milli = r;
        Ok(())
    }

    /// `set_loop`.
    pub fn set_loop(&mut self, mode: LoopMode) {
        self.loop_mode = mode;
    }

    /// `set_clock`.
    pub fn set_clock(&mut self, clock: ClockMode) {
        self.clock = clock;
        self.lock_base = self.t_ns;
        self.lock_steps = 0;
        self.audio_anchor = None;
    }

    fn total_frames(&self) -> Option<u64> {
        self.clip_ns.map(|c| c.div_ceil(self.frame_ns.max(1)))
    }

    /// `seek { time }` (seconds; negative clamps to 0, past the end clamps to the last picture).
    pub fn seek_time(&mut self, time_s: f64, mode: SeekMode) -> Result<(), MediaError> {
        self.check_seekable()?;
        let target = if time_s.is_finite() && time_s > 0.0 {
            (time_s * 1e9) as u64
        } else {
            0
        };
        let target = match self.clip_ns {
            Some(clip) if target >= clip => clip.saturating_sub(self.frame_ns),
            _ => target,
        };
        self.begin_seek(target, mode);
        Ok(())
    }

    /// `seek { frame }`: constant-frame-rate clips only.
    pub fn seek_frame(&mut self, frame: u64, mode: SeekMode) -> Result<(), MediaError> {
        self.check_seekable()?;
        let constant = self
            .info
            .as_ref()
            .and_then(MediaInfo::video_track)
            .is_some_and(|t| t.default_duration_ns.is_some());
        if !constant {
            return Err(MediaError::bad_state(
                "seek by frame on a variable-rate clip",
                self.state,
            ));
        }
        if self.total_frames().is_some_and(|n| frame >= n) {
            return Err(MediaError::SeekOutOfRange(frame));
        }
        // Aim half a frame early so millisecond-rounded timestamps still land on frame `frame`.
        let target = frame
            .checked_mul(self.frame_ns)
            .ok_or(MediaError::SeekOutOfRange(frame))?
            .saturating_sub(self.frame_ns / 2);
        self.begin_seek(target, mode);
        Ok(())
    }

    fn check_seekable(&self) -> Result<(), MediaError> {
        match self.state {
            PlayState::Closed | PlayState::Failed | PlayState::Opening => {
                Err(MediaError::bad_state("seek", self.state))
            }
            _ => Ok(()),
        }
    }

    fn begin_seek(&mut self, target_in_pass: u64, mode: SeekMode) {
        if self.state != PlayState::Seeking {
            self.resume = match self.state {
                PlayState::Playing | PlayState::Buffering => PlayState::Playing,
                _ => PlayState::Paused,
            };
            self.state = PlayState::Seeking;
        }
        // Coalesce: a newer target replaces an older pending one.
        self.pending_seek = Some((target_in_pass, mode));
        if self.clock == ClockMode::Lockstep {
            self.run_seek(true);
        }
    }

    /// For a driver that owns the time (the sequencer, ADR-2862): restarts the feed at the cue at or
    /// before media time `t_ns`, dropping the audio before it. [`Self::show_at`] then decodes forward.
    /// A finished player becomes paused, so it can play again.
    ///
    /// A seek still pending (`Seeking`, ADR-8831) is taken over: the driver's time is the newest
    /// target, so the older one never reaches the feed. The player leaves `Seeking` for the state
    /// the seek would have resumed, and reports one `Seeked` at `t_ns` so a waiter is not left
    /// hanging.
    pub fn reposition(&mut self, t_ns: u64) {
        if matches!(
            self.state,
            PlayState::Closed | PlayState::Failed | PlayState::Opening
        ) {
            return;
        }
        self.queue.clear();
        self.resync_landing = None;
        (self.feed_done, self.feed_pass, self.loops_done) = (false, 0, 0);
        self.ended_fired = false;
        if self.state == PlayState::Ended {
            self.state = PlayState::Paused;
        }
        let t = t_ns.min(i64::MAX as u64 / 4) as i64;
        if self.state == PlayState::Seeking {
            self.pending_seek = None;
            self.seek_scan = None;
            // `loops_done` is 0 here, so `finish_seek` reports `Seeked` at the driver's time.
            self.t_ns = t;
            self.finish_seek();
        }
        self.landing = Some(t);
        if let Some(f) = self.feed.as_mut() {
            // The same clamped time the timeline stores: a driver-supplied time above it is one
            // the player's own clock cannot represent, so the feed must not be asked for it.
            f.seek_accurate(t as u64);
        }
    }

    /// Presents what a linear play shows at media time `t_ns`, in any play state: the picture with
    /// the greatest presentation time at or before it. The clock is the caller's, so a time before
    /// the presented picture repositions the feed. Pulls at most `budget` pictures (blocking for
    /// each with `block`) and returns whether the picture shown is that one for certain.
    pub fn show_at(&mut self, t_ns: u64, block: bool, budget: u32) -> bool {
        if matches!(
            self.state,
            PlayState::Closed | PlayState::Failed | PlayState::Opening | PlayState::Seeking
        ) {
            return false;
        }
        let t = t_ns.min(i64::MAX as u64 / 4) as i64;
        if self.landing.map_or(self.presented_abs > t, |target| t < target) {
            self.reposition(t_ns);
        }
        self.t_ns = t;
        let landing = self.landing.is_some();
        let (mut chosen, mut pulled, mut skipped) = (None, 0u32, 0u64);
        let sure = loop {
            match self.queue.front() {
                // The first picture after a reposition always lands: nothing earlier exists.
                Some(p) if p.pts_ns <= t || (landing && chosen.is_none()) => {
                    skipped += u64::from(chosen.is_some());
                    chosen = self.queue.pop_front();
                }
                Some(_) => break true,
                None if pulled >= budget => break false,
                None => {
                    pulled += 1;
                    match self.pull_feed(block) {
                        Some(p) => self.queue.push_back(p),
                        None => break self.feed_done,
                    }
                }
            }
        };
        if !block && !landing {
            self.stats.frames_dropped += skipped;
        }
        if let Some(p) = chosen {
            self.present(p);
        }
        sure && self.state != PlayState::Failed
    }

    /// `step_frames(n)` on a paused (or ready) player: presents exactly `n` further pictures.
    pub fn step_frames(&mut self, n: u32) -> Result<(), MediaError> {
        match self.state {
            PlayState::Paused | PlayState::Ready => {}
            _ => return Err(MediaError::bad_state("step", self.state)),
        }
        for _ in 0..n {
            match self.pull(true) {
                Some(p) => {
                    self.t_ns = p.pts_ns;
                    self.present(p);
                }
                None => {
                    if self.state == PlayState::Failed {
                        return Ok(());
                    }
                    self.finish();
                    break;
                }
            }
        }
        self.state = if self.state == PlayState::Ended {
            PlayState::Ended
        } else {
            PlayState::Paused
        };
        self.lock_base = self.t_ns;
        self.lock_steps = 0;
        Ok(())
    }

    /// `close`: stops the session, clears the picture (the texture shows `clear_color`).
    pub fn close(&mut self) {
        if self.state == PlayState::Closed {
            return;
        }
        if let Some(c) = self.cues.as_mut() {
            c.end_all(&mut self.events);
        }
        let events = std::mem::take(&mut self.events);
        let rate = self.rate_milli;
        *self = Self::new();
        self.events = events;
        self.rate_milli = rate;
        self.events.push(VideoEvent::Closed);
    }

    /// Skip (the `skip_video` action): closes and reports `Ended { skipped: true }`.
    pub fn skip(&mut self) -> bool {
        if matches!(self.state, PlayState::Closed | PlayState::Failed) {
            return false;
        }
        self.events.push(VideoEvent::Ended { skipped: true });
        self.close();
        true
    }

    // ---- internals ------------------------------------------------------------------------------

    fn fail(&mut self, err: MediaError) {
        if let Some(c) = self.cues.as_mut() {
            c.end_all(&mut self.events);
        }
        self.events.push(VideoEvent::Failed {
            code: err.code().to_string(),
        });
        self.last_error = Some(err);
        self.state = PlayState::Failed;
        self.queue.clear();
    }

    fn clip_i64(&self) -> i64 {
        self.clip_ns.unwrap_or(u64::MAX / 4).min(i64::MAX as u64 / 4) as i64
    }

    /// Pulls the next picture onto the pass timeline, rewinding the feed for the next loop pass when
    /// the current one ends. `None` at the final end (or on failure).
    fn pull(&mut self, block: bool) -> Option<DecodedPicture> {
        if let Some(p) = self.queue.pop_front() {
            return Some(p);
        }
        self.pull_feed(block)
    }

    fn pull_feed(&mut self, block: bool) -> Option<DecodedPicture> {
        let mut rewinds = 0;
        loop {
            if self.feed_done {
                return None;
            }
            let poll = self.feed.as_mut().map_or(FeedPoll::End, |f| f.poll(block));
            if let Some(f) = self.feed.as_ref() {
                let s: FeedStats = f.stats();
                self.stats.packets_read = s.packets_read;
                self.stats.frames_corrupt = s.video.corrupt;
                self.stats.decode_ms_ema = s.video.decode_ms_ema;
                if s.video.decode_too_slow && !self.stats.decode_too_slow {
                    self.stats.decode_too_slow = true;
                    self.slow_warning = true;
                }
            }
            match poll {
                FeedPoll::Frame(mut p) => {
                    self.last_feed_pts = p.pts_ns;
                    p.pts_ns = p
                        .pts_ns
                        .saturating_add(i64::from(self.feed_pass).saturating_mul(self.clip_i64()));
                    return Some(p);
                }
                FeedPoll::Pending => return None,
                FeedPoll::Failed(e) => {
                    self.fail(e);
                    return None;
                }
                FeedPoll::End => {
                    if self.clip_ns.is_none() {
                        self.clip_ns = Some((self.last_feed_pts.max(0) as u64).saturating_add(self.frame_ns));
                    }
                    rewinds += 1;
                    if self.feed_pass.saturating_add(1) < self.effective_passes() && rewinds < 2 {
                        self.feed_pass += 1;
                        if let Some(f) = self.feed.as_mut() {
                            f.seek(0);
                        }
                        continue;
                    }
                    self.feed_done = true;
                    return None;
                }
            }
        }
    }

    /// Passes the player may make over the clip: one under an `External` clock (the host owns time and
    /// nothing loops the decoder), else the loop mode's count.
    fn effective_passes(&self) -> u32 {
        if self.clock == ClockMode::External {
            1
        } else {
            self.loop_mode.passes()
        }
    }

    fn present(&mut self, p: DecodedPicture) {
        self.landing = None;
        self.presented_abs = p.pts_ns;
        self.presented = Some(p);
        self.stats.frames_presented += 1;
    }

    fn finish_opening(&mut self, block: bool) {
        if let Some((target, mode)) = self.pending_seek.take() {
            self.seek_feed(target, mode);
            self.seek_scan = Some((target, mode));
        }
        if let Some((target, mode)) = self.seek_scan {
            // The phase-offset seek may take several ticks on the worker path.
            if self.seek_to_first(target, mode, block) {
                self.seek_scan = None;
            }
        } else if let Some(p) = self.pull(block) {
            self.t_ns = p.pts_ns.max(0);
            self.present(p);
        }
        if self.state == PlayState::Failed {
            return;
        }
        if self.presented.is_none() {
            if self.feed_done {
                // A clip without a decodable picture.
                self.fail(MediaError::DecodeFailed("no decodable picture".into()));
            }
            return;
        }
        self.events.push(VideoEvent::Opened);
        self.state = PlayState::Ready;
        if self.autoplay {
            self.start_playing();
        }
    }

    /// Pulls until the first picture at or after `target` (Accurate) or the first picture (Keyframe)
    /// and presents it. Returns whether a picture was found.
    fn seek_to_first(&mut self, target: u64, mode: SeekMode, block: bool) -> bool {
        let pass_base = i64::from(self.feed_pass).saturating_mul(self.clip_i64());
        loop {
            let Some(p) = self.pull(block) else {
                return false;
            };
            let in_pass = p.pts_ns - pass_base;
            let next_due = self.queue.is_empty() && self.feed_done;
            if mode == SeekMode::Keyframe || in_pass >= target as i64 || next_due {
                self.t_ns = p.pts_ns;
                self.present(p);
                return true;
            }
        }
    }

    /// Seeks the feed. An accurate seek also drops the decoded audio before `target`: its
    /// pictures are discarded by [`Self::seek_to_first`], so that audio must not be heard either
    /// (concurrency audit 11-03 audio H2).
    fn seek_feed(&mut self, target: u64, mode: SeekMode) {
        if let Some(f) = self.feed.as_mut() {
            match mode {
                SeekMode::Accurate => f.seek_accurate(target),
                SeekMode::Keyframe => f.seek(target),
            }
        }
    }

    fn run_seek(&mut self, block: bool) {
        let Some((target, mode)) = self.pending_seek else {
            return;
        };
        self.pending_seek = None;
        self.queue.clear();
        self.resync_landing = None;
        self.feed_done = false;
        self.feed_pass = self.loops_done;
        self.seek_feed(target, mode);
        if self.seek_to_first(target, mode, block) {
            self.finish_seek();
        } else if self.state != PlayState::Failed && !self.seek_ran_out() {
            // Real time: keep catching up on later ticks.
            self.seek_scan = Some((target, mode));
        }
    }

    /// A seek that reached the end of the stream with no picture (past the last keyframe) can never
    /// finish: fail rather than stay in `Seeking` with no event.
    fn seek_ran_out(&mut self) -> bool {
        if !self.feed_done || !self.queue.is_empty() {
            return false;
        }
        self.seek_scan = None;
        self.fail(MediaError::DecodeFailed(
            "no decodable picture after the seek".into(),
        ));
        true
    }

    fn finish_seek(&mut self) {
        let time = self.position_ns() as f64 / 1e9;
        self.events.push(VideoEvent::Seeked { time });
        self.ended_fired = false;
        self.state = self.resume;
        self.lock_base = self.t_ns;
        self.lock_steps = 0;
        self.audio_anchor = None;
        self.starve_ns = 0;
    }

    fn markers_between(&mut self, from_ns: i64, to_ns: i64) {
        if to_ns <= from_ns {
            return;
        }
        let crossed: Vec<Marker> = self
            .info
            .as_ref()
            .map(|i| {
                i.markers
                    .iter()
                    .filter(|m| (m.time_ns as i64) > from_ns && (m.time_ns as i64) <= to_ns)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for m in crossed {
            self.events.push(VideoEvent::Marker { name: m.name });
        }
    }

    /// Advances the clock by one tick and presents the due picture (the `Update` step), then enters
    /// and leaves text cues at the new position.
    pub fn tick(&mut self, input: ClockInput) {
        self.tick_clock(input);
        if self.cues.is_some() && !matches!(self.state, PlayState::Closed | PlayState::Failed) {
            let pos = self.position_ns();
            let clip = self.clip_ns.map(|c| c.min(i64::MAX as u64) as i64);
            if let Some(c) = self.cues.as_mut() {
                c.update(pos, clip, &mut self.events);
            }
        }
    }

    fn tick_clock(&mut self, input: ClockInput) {
        let block = self.clock == ClockMode::Lockstep;
        match self.state {
            PlayState::Closed
            | PlayState::Failed
            | PlayState::Ended
            | PlayState::Ready
            | PlayState::Paused => {
                return;
            }
            PlayState::Opening => {
                self.finish_opening(block);
                return;
            }
            PlayState::Seeking => {
                if self.pending_seek.is_some() {
                    self.run_seek(block);
                } else if let Some((target, mode)) = self.seek_scan {
                    if self.seek_to_first(target, mode, block) {
                        self.seek_scan = None;
                        self.finish_seek();
                    } else if self.state != PlayState::Failed {
                        self.seek_ran_out();
                    }
                }
                return;
            }
            PlayState::Buffering => {
                while self.queue.len() < self.resume_frames {
                    match self.pull_feed(false) {
                        Some(p) => self.queue.push_back(p),
                        None => break,
                    }
                }
                if self.state == PlayState::Buffering
                    && (self.queue.len() >= self.resume_frames || self.feed_done)
                {
                    self.events.push(VideoEvent::BufferingDone);
                    self.start_playing();
                }
                return;
            }
            PlayState::Playing => {}
        }
        if self.game_held {
            return;
        }
        if self.decode_paused && self.clock_frozen {
            self.paused_ticks += 1;
            return;
        }
        let prev_t = self.t_ns;
        let prev_pos = self.position_ns();
        let rate = i128::from(self.rate_milli);
        self.t_ns = match input {
            ClockInput::Step { dt_ns } => {
                let t = lockstep_time_ns(self.lock_base, self.lock_steps, dt_ns, self.rate_milli);
                self.lock_steps += 1;
                t
            }
            ClockInput::Elapsed { dt_ns } => {
                let adv = (i128::from(dt_ns) * rate / 1000).min(i128::from(i64::MAX / 4)) as i64;
                self.t_ns.saturating_add(adv)
            }
            ClockInput::Time { t_ns } => t_ns.min(i64::MAX as u64 / 4) as i64,
            ClockInput::Audio { frames, sample_rate } => {
                let (f0, t0) = *self.audio_anchor.get_or_insert((frames, self.t_ns));
                let consumed = i128::from(frames.saturating_sub(f0));
                let adv = consumed * 1_000_000_000 * rate / 1000 / i128::from(sample_rate.max(1));
                t0.saturating_add(adv.min(i128::from(i64::MAX / 4)) as i64)
            }
        };
        let t = self.t_ns;
        let clip = self.clip_i64();
        let (loops_before, events_before) = (self.loops_done, self.events.len());
        let passes = self.effective_passes();
        let wraps = loops_at(t, clip, passes);
        if wraps > self.loops_done {
            // Counted, not stepped: a short clip on a long tick crosses many wraps at once, and only
            // the last `MAX_LOOPED_EVENTS_PER_TICK` of them are reported (deep-zai-media finding 1).
            let first_reported = wraps
                .saturating_sub(MAX_LOOPED_EVENTS_PER_TICK as u32 - 1)
                .max(self.loops_done + 1);
            self.loops_done = wraps;
            self.events
                .extend((first_reported..=wraps).map(|count| VideoEvent::Looped { count }));
        }
        let pos = self.position_ns();
        if pos >= prev_pos {
            self.markers_between(prev_pos, pos);
        } else {
            self.markers_between(prev_pos, clip);
            self.markers_between(-1, pos);
        }

        if self.decode_paused {
            // `PauseDecode`: the clock runs, nothing is pulled or presented.
            self.paused_ticks += 1;
            return;
        }
        if self.resync {
            // Back from a decode pause: restart the feed at the cue at or before the current time;
            // the rule below then discards the older pictures (not counted as drops).
            self.resync = false;
            self.queue.clear();
            self.feed_done = false;
            self.feed_pass = self.loops_done;
            // Spec 50 § 2.7: an accurate seek to the current time, so the decoded audio between the
            // cue point and the clock is dropped too (deep-zai-media finding 2).
            let target = self.position_ns().max(0) as u64;
            self.seek_feed(target, SeekMode::Accurate);
            self.quiet_drops = true;
            self.resync_landing = Some(0);
        }
        if self.half_rate && self.clock != ClockMode::Lockstep {
            self.half_rate_skip = !self.half_rate_skip;
            if self.half_rate_skip {
                return;
            }
        }
        // Presentation rule: the greatest presentation time <= t.
        let mut chosen: Option<DecodedPicture> = None;
        let mut skipped = 0u64;
        let mut waiting = false;
        loop {
            if let Some(front) = self.queue.front() {
                if front.pts_ns <= t {
                    if chosen.is_some() {
                        skipped += 1;
                    }
                    chosen = self.queue.pop_front();
                    continue;
                }
                break;
            }
            match self.pull_feed(block) {
                Some(p) => self.queue.push_back(p),
                None => {
                    waiting = !self.feed_done;
                    break;
                }
            }
        }
        if self.state == PlayState::Failed {
            return;
        }
        if let Some(ticks) = self.resync_landing {
            // The accurate landing: a picture is the clock's only once a later one (or the end) is
            // known. Until then the newest due picture waits at the queue's front and the picture
            // shown before the pause stays, so an older one never flashes up.
            if waiting && chosen.is_some() && ticks < RESYNC_LANDING_TICKS {
                self.resync_landing = Some(ticks + 1);
                if let Some(p) = chosen.take() {
                    self.queue.push_front(p);
                }
            } else if chosen.is_some() {
                self.resync_landing = None;
            } else {
                self.resync_landing = Some(ticks.saturating_add(1));
            }
        }
        match chosen {
            Some(p) => {
                if self.clock != ClockMode::Lockstep && !self.quiet_drops {
                    self.stats.frames_dropped += skipped;
                }
                self.quiet_drops = false;
                self.present(p);
                self.starve_ns = 0;
            }
            None => {
                let held_past_period = t >= self.presented_abs.saturating_add(self.frame_ns as i64);
                let at_end = self.feed_done && self.queue.is_empty();
                if held_past_period && !at_end {
                    self.stats.frames_repeated += 1;
                    if self.clock != ClockMode::Lockstep && self.queue.is_empty() {
                        self.starve_ns += (t - prev_t).max(0) as u64;
                        if self.starve_ns > self.starve_limit_ns {
                            self.state = PlayState::Buffering;
                            self.t_ns = prev_t;
                            // The rewind undoes this tick's loop crossings and their events too.
                            self.loops_done = loops_before;
                            self.events.truncate(events_before);
                            self.events.push(VideoEvent::BufferingStarted);
                            return;
                        }
                    }
                }
            }
        }
        let end_t = i64::from(self.loops_done.saturating_add(1)).saturating_mul(clip);
        if self.feed_done
            && self.queue.is_empty()
            && t >= end_t
            && self.loops_done.saturating_add(1) >= passes
        {
            self.finish();
        }
    }

    fn finish(&mut self) {
        if !self.ended_fired {
            self.ended_fired = true;
            self.events.push(VideoEvent::Ended { skipped: false });
        }
        self.state = PlayState::Ended;
    }
}

/// Index of the frame at `pts_ns` at a nominal `frame_ns`, rounded to nearest. Saturates: a container
/// can set a timestamp to `i64::MAX`.
fn frame_index_at(pts_ns: i64, frame_ns: u64) -> u64 {
    let half = (frame_ns / 2) as i64;
    (pts_ns.max(0).saturating_add(half) / frame_ns.max(1) as i64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Media audit 1: a hostile timestamp is a large index, not an overflow.
    #[test]
    fn frame_index_saturates_on_a_hostile_timestamp() {
        assert_eq!(frame_index_at(0, 40), 0);
        assert_eq!(frame_index_at(79, 40), 2);
        assert_eq!(frame_index_at(-5, 40), 0);
        assert_eq!(frame_index_at(i64::MAX, 40), (i64::MAX / 40) as u64);
        assert_eq!(frame_index_at(i64::MAX, 0), i64::MAX as u64);
    }
}
