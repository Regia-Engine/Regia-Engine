// SPDX-License-Identifier: MPL-2.0
//! Video sections (`.seq.ron` v4, ADR-2862): one `regia_media` session per video section the
//! player's clock is inside, slaved to the sequence time.
//!
//! Each frame, after the tracks apply and before the pictures are published
//! (`regia_render::VideoSystems::Publish`), every player's video sections are compared with the
//! sessions it runs ([`SequenceVideos`], keyed by the section's path in the tree): a section the
//! clock entered opens its source and shows the picture of the clock's source time, a section the
//! clock left closes and hands its target back, and an edit that changes a running section's source
//! or target reopens it. A player that stops, finishes under `EndPolicy::Once` or despawns closes
//! its sessions.
//!
//! **Which picture.** The source time at sequence time `t` is `offset + (t - start)` in the
//! section's own sequence time (so the speed of a sub-sequence section above it applies), wrapped
//! over the source's length when the section loops. The picture shown is the one with the greatest
//! presentation time at or before the source time plus half a millisecond, because WebM timestamps
//! are rounded to the millisecond: without it a frame whose stamp rounds up would show one output
//! frame late, and the frame before it twice.
//!
//! **Clock.** A session's player is on `ClockMode::External`. This system writes the time each
//! frame and presents through `PlayerCore::show_at`, so the picture is the same frame's, and a time
//! behind the picture restarts the feed at the previous keyframe and decodes forward. The clock
//! never reads the audio: a sequence is one master clock. Its own audio follows by
//! [drift correction](needs_resync): when the shown picture has fallen more than
//! [`RESYNC_VIDEO_DRIFT`] behind (the decoder is too slow) or the audio more than
//! [`RESYNC_AUDIO_DRIFT`] from the clock (a time scale, a clock that hitched), the session
//! repositions, at most once per [`RESYNC_COOLDOWN`] of source time.
//!
//! **Scrub.** A paused player's sections stay open with their sound held, and follow the clock.
//! Real time never blocks: a seek decodes on the session's thread and the picture lands within
//! a few frames, and each call pulls at most [`SequenceVideos::realtime_budget`] pictures.
//!
//! **Frame-exact mode.** While the app's clock is manual (`TimeUpdateStrategy::ManualDuration`:
//! render-out, `play.step`, headless tests) or [`SequenceVideos::force_exact`] is on, sessions
//! decode on the calling thread and this system blocks until the exact picture of each frame is
//! decoded, up to [`SequenceVideos::exact_budget`] pictures a frame (past it the issue
//! `seq_video_failed` with code `seek_bound` says so, and the next frame goes on from there). No
//! picture is ever dropped or late and nothing depends on the wall clock, so two runs show the same
//! pictures.
//!
//! **Targets.** A fullscreen layer (`VideoFullscreen`), the bound entity's own `VideoPlayer` (taken
//! over and restored), a bound mesh's albedo or emissive material slot (`VideoMaterial`, material
//! restored) and a bound UI image node (`UiVideo`, image restored). All but the player target need
//! `regia_media::VideoFramePlugin`, and the UI target also `regia_ui::video::UiVideoPlugin`.
//!
//! **Problems.** A missing source reports [`SequenceIssue::VideoMissing`] (`seq_video_missing`) and
//! any other failure [`SequenceIssue::VideoFailed`] (`seq_video_failed`); the section shows nothing
//! and the rest of the sequence plays. Without the `media` feature, `regia_media::MediaPlugin` or
//! the frame plugin a target needs, [`SequenceIssue::VideoUnavailable`] (`seq_video_unavailable`).

use regia_app::prelude::*;

use super::player::{SequenceIssue, SequencePlayer, SequencerSet};

/// A shown picture may be at most this far behind the clock, seconds, before a session repositions.
pub const RESYNC_VIDEO_DRIFT: f64 = 0.25;
/// The session's audio may be at most this far from the clock, seconds, before it repositions.
pub const RESYNC_AUDIO_DRIFT: f64 = 0.1;
/// Source seconds between two repositions of drift correction, so a decoder too slow for the clip
/// does not seek forever.
pub const RESYNC_COOLDOWN: f64 = 1.0;
/// A forward move of the clock past this many seconds repositions instead of decoding forward.
pub const FORWARD_SEEK: f64 = 1.0;
/// Half the millisecond WebM timestamps are rounded to, seconds.
pub const TIMESTAMP_SLACK: f64 = 0.0005;

/// The state of one video section in effect (`sequencer.video_sections`).
#[derive(Clone, Debug, PartialEq)]
pub struct VideoSectionStatus {
    /// The section's place in the tree: a `[track, section]` pair per level.
    pub path: Vec<usize>,
    /// The name of the track it is on.
    pub track: String,
    /// The source's project path.
    pub video: String,
    /// The target kind (`fullscreen`, `player`, `screen`, `ui_image`).
    pub target: &'static str,
    /// The entity of the session's video player (`None` when the source did not open).
    pub media: Option<Entity>,
    /// The session's state name (`playing`, `paused`, `seeking`, ...; `closed` with no session).
    pub state: String,
    /// The source time the clock asks for, seconds.
    pub source_time: f64,
    /// The index of the picture shown at the source's nominal rate (`None` before the first).
    pub frame_shown: Option<u64>,
    /// The presentation time of the picture shown, seconds.
    pub picture_time: Option<f64>,
    /// How far the picture shown is behind the clock past its own frame period, seconds: 0 when
    /// the right picture shows.
    pub drift: f64,
    /// How far the session's audio is from the clock, seconds (`None` without an audio stream).
    pub audio_drift: Option<f64>,
    /// The picture shown is known to be the one of `source_time`.
    pub certain: bool,
    /// The session runs frame-exact (blocking on the decoder).
    pub exact: bool,
    /// Repositions for a clock that jumped (a seek, a scrub, a loop wrap).
    pub seeks: u64,
    /// Repositions by drift correction.
    pub resyncs: u64,
    /// Due pictures the session skipped because it was late (never in frame-exact mode).
    pub frames_dropped: u64,
    /// The media error code, when the session failed.
    pub error: Option<String>,
}

/// The sessions sequence players run, per player entity and section path. A resource the
/// [`super::SequencerPlugin`] keeps.
#[derive(Resource, Debug)]
pub struct SequenceVideos {
    /// Decode frame-exact even under the wall clock (the manual clock turns it on by itself).
    pub force_exact: bool,
    /// Most pictures one real-time frame pulls for one section.
    pub realtime_budget: u32,
    /// Most pictures one frame-exact frame decodes for one section.
    pub exact_budget: u32,
    #[cfg(feature = "media")]
    players: std::collections::BTreeMap<Entity, driver::PlayerVideos>,
    opened: u64,
    closed: u64,
}

impl Default for SequenceVideos {
    fn default() -> Self {
        Self {
            force_exact: false,
            realtime_budget: 256,
            exact_budget: 20_000,
            #[cfg(feature = "media")]
            players: std::collections::BTreeMap::new(),
            opened: 0,
            closed: 0,
        }
    }
}

impl SequenceVideos {
    /// The sections `player` has in effect, by section path.
    #[must_use]
    pub fn sections(&self, player: Entity) -> Vec<VideoSectionStatus> {
        #[cfg(feature = "media")]
        if let Some(entry) = self.players.get(&player) {
            return entry.running.values().map(|r| r.status.clone()).collect();
        }
        let _ = player;
        Vec::new()
    }

    /// Sessions opened so far (a section whose source failed to open counts).
    #[must_use]
    pub fn opened(&self) -> u64 {
        self.opened
    }

    /// Sessions closed so far.
    #[must_use]
    pub fn closed(&self) -> u64 {
        self.closed
    }

    /// `true` when no player has a section in effect.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "media")]
        return self.players.values().all(|p| p.running.is_empty());
        #[cfg(not(feature = "media"))]
        true
    }
}

/// The linear gain of a section's `volume_db`: silent at -80 dB and below.
#[must_use]
pub fn volume_gain(volume_db: f32) -> f32 {
    if volume_db <= -80.0 {
        0.0
    } else {
        10f32.powf(volume_db / 20.0)
    }
}

/// The media time, nanoseconds, a section at source position `position` seconds shows: half a
/// millisecond of timestamp rounding added, then wrapped over the source's length when it loops.
#[must_use]
pub fn media_time_ns(position: f64, looping: bool, duration: Option<f64>) -> u64 {
    let late = position + TIMESTAMP_SLACK;
    let wrapped = match duration.filter(|d| *d > 0.0) {
        Some(d) if looping => late.rem_euclid(d),
        _ => late,
    };
    if wrapped.is_finite() && wrapped > 0.0 {
        (wrapped * 1e9) as u64
    } else {
        0
    }
}

/// Whether drift correction should reposition a playing session now: the picture has fallen more
/// than [`RESYNC_VIDEO_DRIFT`] behind or the audio is more than [`RESYNC_AUDIO_DRIFT`] from the
/// clock, and [`RESYNC_COOLDOWN`] has passed since the last time. Pure.
#[must_use]
pub fn needs_resync(video_drift: f64, audio_drift: Option<f64>, since_last: f64) -> bool {
    since_last >= RESYNC_COOLDOWN
        && (video_drift > RESYNC_VIDEO_DRIFT || audio_drift.is_some_and(|d| d.abs() > RESYNC_AUDIO_DRIFT))
}

fn track_in_effect(player: &SequencePlayer) -> Vec<String> {
    let frame = player.frame();
    let mut tracks: Vec<String> = frame.videos.iter().map(|v| v.track.to_string()).collect();
    tracks.dedup();
    tracks
}

/// Reports that this build or app cannot play the video sections in effect.
fn report_unavailable(player: &mut SequencePlayer, reason: &str) {
    for track in track_in_effect(player) {
        player.report(SequenceIssue::VideoUnavailable {
            track,
            reason: reason.to_string(),
        });
    }
}

#[cfg(not(feature = "media"))]
fn report_no_media(mut players: Query<&mut SequencePlayer>) {
    for mut player in &mut players {
        if player.has_video_sections() && player.is_active() {
            report_unavailable(&mut player, "this build has no video: enable the `media` feature");
        }
    }
}

/// Adds [`SequenceVideos`] and the sync (`PostUpdate`, after [`SequencerSet::Apply`] and before the
/// pictures are published).
pub(crate) fn install(app: &mut App) {
    app.init_resource::<SequenceVideos>();
    #[cfg(feature = "media")]
    app.add_systems(
        PostUpdate,
        driver::sync_videos
            .after(SequencerSet::Apply)
            .before(regia_render::VideoSystems::Publish)
            .run_if(driver::videos_wanted),
    );
    #[cfg(not(feature = "media"))]
    app.add_systems(
        PostUpdate,
        report_no_media
            .after(SequencerSet::Apply)
            .run_if(any_with_component::<SequencePlayer>),
    );
}

#[cfg(feature = "media")]
pub use driver::{VideoThumbnail, video_thumbnail};

#[cfg(feature = "media")]
mod driver;
