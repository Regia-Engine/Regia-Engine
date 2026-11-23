// SPDX-License-Identifier: MPL-2.0
//! The session driver of video sections (see the parent module's docs).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_audio::media_feed::{MEDIA_FEED_RATES, MediaAudioRegistry};
use regia_media::container::MediaInfo;
use regia_media::frame_source::VideoMaterial;
use regia_media::player::ClockInput;
use regia_media::plugin::{MediaFiles, MediaSettings, VisibilityPolicy, open_player_pooled};
use regia_media::reference::picture_to_rgba8;
use regia_media::session::{Session, SessionOptions};
use regia_media::{
    ClockMode, Fit, LoopMode, MediaError, PlayState, PlayerCore, VideoFullscreen, VideoPlayer,
    VideoPlayerState,
};
use regia_render::{CpuMaterial, MeshRenderer, VideoFrames, VideoSlot};
use regia_scene::components::Name;
use regia_ui::node::UiImage;
use regia_ui::video::{UiVideo, UiVideoStats};

use super::super::asset::{VideoFit, VideoScreenSlot, VideoTarget};
use super::super::player::{ClockSource, PlaybackStatus, SequenceIssue, SequencePlayer};
use super::{
    FORWARD_SEEK, SequenceVideos, TIMESTAMP_SLACK, VideoSectionStatus, media_time_ns, needs_resync,
    report_unavailable, volume_gain,
};

/// How a section gives its target back when it closes.
#[derive(Debug)]
enum Restore {
    /// The session's player is its own entity: despawn it.
    Spawned,
    /// The bound entity's own player: put back what it was (`None`: it had none).
    Player(Option<VideoPlayer>),
    /// The bound mesh and its material before the section.
    Screen(Entity, Option<Arc<CpuMaterial>>),
    /// The bound UI node and its image before the section.
    Ui(Entity, Option<UiImage>),
}

/// One open video section.
#[derive(Debug)]
pub(super) struct RunningVideo {
    pub(super) status: VideoSectionStatus,
    source: String,
    target: VideoTarget,
    binding: Option<Entity>,
    /// `None` when the source did not open.
    media: Option<Entity>,
    restore: Restore,
    /// The last media time shown, nanoseconds.
    last_ns: u64,
    last_resync: f64,
    /// (frames the audio stream had mixed, source seconds) when last in step.
    audio_anchor: Option<(u64, f64)>,
    volume_db: f32,
}

#[derive(Debug, Default)]
pub(super) struct PlayerVideos {
    pub(super) running: BTreeMap<Vec<usize>, RunningVideo>,
}

/// One section the clock is inside, owned so the world is free while the session opens.
struct Wanted {
    path: Vec<usize>,
    track: String,
    video: String,
    target: VideoTarget,
    binding: Option<Entity>,
    position: f64,
    rate: f64,
    volume_db: f32,
    looping: bool,
}

/// What one player asks of its sessions this frame.
struct PlayerFrame {
    entity: Entity,
    wanted: Vec<Wanted>,
    /// The clock runs: the sessions play their sound.
    playing: bool,
}

/// Runs the sync while a player has video sections or a session is still open.
pub(super) fn videos_wanted(players: Query<&SequencePlayer>, videos: Res<SequenceVideos>) -> bool {
    !videos.is_empty() || players.iter().any(SequencePlayer::has_video_sections)
}

fn player_frame(entity: Entity, player: &SequencePlayer, delta_zero: bool) -> PlayerFrame {
    let mut wanted: Vec<Wanted> = Vec::new();
    if player.is_active() && player.has_video_sections() {
        let frame = player.frame();
        for video in &frame.videos {
            let binding = video
                .binding
                .and_then(|name| player.target_entities().get(name).copied());
            // An unbound target is already reported as one; its section waits.
            if video.binding.is_some() && binding.is_none() {
                continue;
            }
            // Two sections on one entity: the later one in the tree has it.
            if binding.is_some() {
                wanted.retain(|w| w.binding != binding);
            }
            wanted.push(Wanted {
                path: video.path.clone(),
                track: video.track.to_string(),
                video: video.video.to_string(),
                target: video.target.clone(),
                binding,
                position: video.position,
                rate: video.rate,
                volume_db: video.volume_db,
                looping: video.looping,
            });
        }
    }
    let frozen = player.clock == ClockSource::Gameplay && delta_zero;
    PlayerFrame {
        entity,
        wanted,
        playing: player.status() == PlaybackStatus::Playing && !player.hold_audio && !frozen,
    }
}

/// The sections of every player against their sessions.
pub(super) fn sync_videos(world: &mut World) {
    let _span = tracing::info_span!(target: "regia_framework", "sequencer_video").entered();
    let Some(mut videos) = world.remove_resource::<SequenceVideos>() else {
        return;
    };
    let delta_zero = world.get_resource::<Time>().is_none_or(|t| t.delta().is_zero());
    let manual = matches!(
        world.get_resource::<TimeUpdateStrategy>(),
        Some(TimeUpdateStrategy::ManualDuration(_))
    );
    let exact = videos.force_exact || manual;
    let mut query = world.query::<(Entity, &SequencePlayer)>();
    let mut frames: Vec<PlayerFrame> = query
        .iter(world)
        .map(|(entity, player)| player_frame(entity, player, delta_zero))
        .collect();
    frames.sort_unstable_by_key(|f| f.entity.to_bits());
    // Players that are gone close their sessions.
    let gone: Vec<Entity> = videos
        .players
        .keys()
        .copied()
        .filter(|e| !frames.iter().any(|f| f.entity == *e))
        .collect();
    for entity in gone {
        if let Some(entry) = videos.players.remove(&entity) {
            for running in entry.running.into_values() {
                close_section(world, running);
                videos.closed += 1;
            }
        }
    }
    let media_installed = world.contains_resource::<MediaSettings>();
    for frame in frames {
        if !media_installed {
            if let Some(mut player) = world.get_mut::<SequencePlayer>(frame.entity) {
                report_unavailable(&mut player, "this app has no video: add regia_media::MediaPlugin");
            }
            continue;
        }
        sync_player(world, &mut videos, frame, exact);
    }
    world.insert_resource(videos);
}

fn sync_player(world: &mut World, videos: &mut SequenceVideos, frame: PlayerFrame, exact: bool) {
    let mut entry = videos.players.remove(&frame.entity).unwrap_or_default();
    // Sections the clock left, or that were edited to something else.
    let stale: Vec<Vec<usize>> = entry
        .running
        .iter()
        .filter(|(path, running)| {
            !frame.wanted.iter().any(|w| {
                &&w.path == path
                    && w.video == running.source
                    && w.target == running.target
                    && w.binding == running.binding
            }) || running.status.exact != exact
        })
        .map(|(path, _)| path.clone())
        .collect();
    for path in stale {
        if let Some(running) = entry.running.remove(&path) {
            close_section(world, running);
            videos.closed += 1;
        }
    }
    let budget = if exact {
        videos.exact_budget
    } else {
        videos.realtime_budget
    };
    for wanted in &frame.wanted {
        if !entry.running.contains_key(&wanted.path) {
            let running = open_section(world, frame.entity, wanted, exact);
            entry.running.insert(wanted.path.clone(), running);
            videos.opened += 1;
        }
        if let Some(running) = entry.running.get_mut(&wanted.path) {
            let issue = drive_section(world, running, wanted, frame.playing, budget);
            if let (Some(issue), Some(mut player)) = (issue, world.get_mut::<SequencePlayer>(frame.entity)) {
                player.report(issue);
            }
        }
    }
    if !entry.running.is_empty() {
        videos.players.insert(frame.entity, entry);
    }
}

fn media_fit(fit: VideoFit) -> Fit {
    match fit {
        VideoFit::Letterbox => Fit::Letterbox,
        VideoFit::Fill => Fit::Fill,
        VideoFit::Stretch => Fit::Stretch,
    }
}

fn video_slot(slot: VideoScreenSlot) -> VideoSlot {
    match slot {
        VideoScreenSlot::Albedo => VideoSlot::Albedo,
        VideoScreenSlot::Emissive => VideoSlot::Emissive,
    }
}

/// What the app lacks to show this target, if anything.
fn target_unavailable(world: &World, target: &VideoTarget) -> Option<&'static str> {
    let frames = || {
        world
            .get_resource::<VideoFrames>()
            .is_some_and(|f| f.source().is_some())
    };
    match target {
        VideoTarget::Player(_) => None,
        VideoTarget::Fullscreen { .. } | VideoTarget::Screen { .. } if !frames() => {
            Some("this target needs regia_media::VideoFramePlugin")
        }
        VideoTarget::UiImage(_) if !frames() || !world.contains_resource::<UiVideoStats>() => {
            Some("a UI target needs regia_media::VideoFramePlugin and regia_ui::video::UiVideoPlugin")
        }
        _ => None,
    }
}

/// Puts the target in place around a session's player and returns how to undo it.
fn attach_target(world: &mut World, wanted: &Wanted, player: VideoPlayer) -> (Entity, Restore) {
    let name = || Name::new(format!("sequence video: {}", wanted.track));
    match (&wanted.target, wanted.binding) {
        (VideoTarget::Player(_), Some(entity)) => {
            let saved = world.get::<VideoPlayer>(entity).cloned();
            world
                .entity_mut(entity)
                .insert((player, VideoPlayerState::default()));
            (entity, Restore::Player(saved))
        }
        (VideoTarget::Screen { slot, .. }, Some(mesh)) => {
            let media = world.spawn((player, name())).id();
            let saved = world.get::<MeshRenderer>(mesh).map(|m| m.material.clone());
            world.entity_mut(mesh).insert(VideoMaterial {
                player: media,
                slot: video_slot(*slot),
            });
            (media, Restore::Screen(mesh, saved))
        }
        (VideoTarget::UiImage(_), Some(node)) => {
            let media = world.spawn((player, name())).id();
            let saved = world.get::<UiImage>(node).copied();
            world.entity_mut(node).insert(UiVideo::new(media.to_bits()));
            (media, Restore::Ui(node, saved))
        }
        (VideoTarget::Fullscreen { layer, fit }, _) => {
            let screen = VideoFullscreen {
                fit: media_fit(*fit),
                layer: *layer,
                skippable: false,
            };
            (world.spawn((player, screen, name())).id(), Restore::Spawned)
        }
        // A bound target always has its entity here (`player_frame`).
        _ => (world.spawn((player, name())).id(), Restore::Spawned),
    }
}

fn open_section(world: &mut World, owner: Entity, wanted: &Wanted, exact: bool) -> RunningVideo {
    let _span = tracing::debug_span!(target: "regia_framework", "sequencer_video_open").entered();
    let mut running = RunningVideo {
        status: VideoSectionStatus {
            path: wanted.path.clone(),
            track: wanted.track.clone(),
            video: wanted.video.clone(),
            target: wanted.target.kind(),
            media: None,
            state: PlayState::Closed.name().to_string(),
            source_time: wanted.position,
            frame_shown: None,
            picture_time: None,
            drift: 0.0,
            audio_drift: None,
            certain: false,
            exact,
            seeks: 0,
            resyncs: 0,
            frames_dropped: 0,
            error: None,
        },
        source: wanted.video.clone(),
        target: wanted.target.clone(),
        binding: wanted.binding,
        media: None,
        restore: Restore::Spawned,
        last_ns: 0,
        last_resync: f64::NEG_INFINITY,
        audio_anchor: None,
        volume_db: wanted.volume_db,
    };
    if let Some(reason) = target_unavailable(world, &wanted.target)
        && let Some(mut player) = world.get_mut::<SequencePlayer>(owner)
    {
        player.report(SequenceIssue::VideoUnavailable {
            track: wanted.track.clone(),
            reason: reason.to_string(),
        });
    }
    let player = VideoPlayer {
        source: wanted.video.clone(),
        autoplay: false,
        // Blocking session: `open_player_pooled` decodes on this thread only for this clock.
        clock: if exact {
            ClockMode::Lockstep
        } else {
            ClockMode::External
        },
        loop_mode: LoopMode::Off,
        volume: volume_gain(wanted.volume_db),
        visibility: VisibilityPolicy::Always,
        ..VideoPlayer::default()
    };
    let mut states = world.query::<&VideoPlayerState>();
    let (open, decoding) = states.iter(world).fold((0, 0), |(open, decoding), state| {
        let core = state.core();
        let live = !matches!(core.state(), PlayState::Closed | PlayState::Failed);
        (
            open + usize::from(live),
            decoding + usize::from(core.holds_decoder()),
        )
    });
    // The bound entity's own session gives way, its sound with it.
    if let (VideoTarget::Player(_), Some(bound)) = (&wanted.target, wanted.binding) {
        if let Some(mut state) = world.get_mut::<VideoPlayerState>(bound) {
            state.core_mut().close();
        }
        if let Some(mut registry) = world.get_resource_mut::<MediaAudioRegistry>() {
            registry.unregister(bound.to_bits());
        }
    }
    let (media, restore) = attach_target(world, wanted, player.clone());
    running.media = Some(media);
    running.status.media = Some(media);
    running.restore = restore;
    let files = world.get_resource::<MediaFiles>().cloned().unwrap_or_default();
    let settings = world.get_resource::<MediaSettings>().cloned().unwrap_or_default();
    let mut registry = world.remove_resource::<MediaAudioRegistry>();
    if let Some(mut state) = world.get_mut::<VideoPlayerState>(media) {
        // A frame-exact session is not a slot of the decode pool: render-out waits for it anyway.
        let decoding = if exact { 0 } else { decoding };
        open_player_pooled(
            media,
            &player,
            &mut state,
            &files,
            &settings,
            (open, decoding),
            registry.as_mut(),
        );
    }
    if let Some(registry) = registry {
        world.insert_resource(registry);
    }
    let error = world
        .get::<VideoPlayerState>(media)
        .and_then(|state| state.core().last_error().cloned());
    if let Some(error) = error {
        tracing::warn!(
            target: "regia_framework",
            video = %wanted.video,
            "video section did not open: {error}"
        );
        let issue = open_issue(&wanted.track, &wanted.video, &error);
        running.status.error = Some(error.code().to_string());
        if let Some(mut owner) = world.get_mut::<SequencePlayer>(owner) {
            owner.report(issue);
        }
        release_target(world, &mut running);
        running.media = None;
        running.status.media = None;
        return running;
    }
    if exact {
        // Decoded here from now on, on the time this system writes.
        if let Some(mut player) = world.get_mut::<VideoPlayer>(media) {
            player.clock = ClockMode::External;
        }
        if let Some(mut state) = world.get_mut::<VideoPlayerState>(media) {
            state.core_mut().set_clock(ClockMode::External);
        }
    }
    let no_mesh = matches!(wanted.target, VideoTarget::Screen { .. })
        && wanted
            .binding
            .is_some_and(|mesh| world.get::<MeshRenderer>(mesh).is_none());
    if let Some(mut owner) = world.get_mut::<SequencePlayer>(owner) {
        owner.forget_video_issues(&wanted.track, &wanted.video);
        if no_mesh {
            owner.report(SequenceIssue::VideoFailed {
                track: wanted.track.clone(),
                video: wanted.video.clone(),
                code: MediaError::MaterialMissingVideoSlot.code().to_string(),
            });
        }
    }
    running
}

fn open_issue(track: &str, video: &str, error: &MediaError) -> SequenceIssue {
    match error {
        MediaError::SourceNotFound(_) => SequenceIssue::VideoMissing {
            track: track.to_string(),
            video: video.to_string(),
        },
        other => SequenceIssue::VideoFailed {
            track: track.to_string(),
            video: video.to_string(),
            code: other.code().to_string(),
        },
    }
}

/// The nominal frame period of a clip, seconds.
fn frame_period(info: Option<&MediaInfo>) -> f64 {
    info.and_then(MediaInfo::video_track)
        .and_then(|t| t.default_duration_ns)
        .filter(|d| *d > 0)
        .map_or(1.0 / 30.0, |d| d as f64 / 1e9)
}

/// Shows the picture of this frame's source time and keeps the session in step. A problem comes back
/// as an issue for the owner.
fn drive_section(
    world: &mut World,
    running: &mut RunningVideo,
    wanted: &Wanted,
    playing: bool,
    budget: u32,
) -> Option<SequenceIssue> {
    running.status.source_time = wanted.position;
    let media = running.media?;
    let exact = running.status.exact;
    let stream = world
        .get_resource::<MediaAudioRegistry>()
        .and_then(|r| r.stream(media.to_bits()).cloned());
    if running.volume_db != wanted.volume_db {
        running.volume_db = wanted.volume_db;
        if let Some(mut registry) = world.get_resource_mut::<MediaAudioRegistry>() {
            registry.set_volume(media.to_bits(), volume_gain(wanted.volume_db));
        }
    }
    let mut state = world.get_mut::<VideoPlayerState>(media)?;
    let core = state.core_mut();
    // The decode thread's first picture: on the way in, the session has nothing to show.
    if core.state() == PlayState::Opening {
        core.tick(ClockInput::Time { t_ns: 0 });
    }
    if core.state() == PlayState::Failed {
        let code = core
            .last_error()
            .map_or("decode_failed", MediaError::code)
            .to_string();
        running.status.state = core.state().name().to_string();
        let first = running.status.error.is_none();
        running.status.error = Some(code.clone());
        return first.then(|| SequenceIssue::VideoFailed {
            track: wanted.track.clone(),
            video: wanted.video.clone(),
            code,
        });
    }
    let duration = core.duration();
    let t_ns = media_time_ns(wanted.position, wanted.looping, duration);
    let source_time = t_ns as f64 / 1e9 - TIMESTAMP_SLACK;
    let opening = core.state() == PlayState::Opening;
    let mut issue = None;
    if !opening {
        let moved = t_ns as f64 / 1e9 - running.last_ns as f64 / 1e9;
        let rewound = moved < 0.0;
        if rewound || moved > FORWARD_SEEK {
            running.status.seeks += 1;
            running.audio_anchor = None;
            core.reposition(t_ns);
        }
        running.last_ns = t_ns;
        let certain = core.show_at(t_ns, exact, budget);
        running.status.certain = certain;
        // A seek still in flight (a game-side seek the decoder has not reached) is not a miss: the
        // section is uncertain until it resolves, then lands on the next frame (ADR-8831).
        let in_flight = core.state() == PlayState::Seeking;
        if exact && !certain && !in_flight && core.state() != PlayState::Failed {
            issue = Some(SequenceIssue::VideoFailed {
                track: wanted.track.clone(),
                video: wanted.video.clone(),
                code: "seek_bound".to_string(),
            });
        }
        // The rate of a sub-sequence section above it reaches the sound; past what the feed plays,
        // the stream mutes itself.
        let rate = wanted.rate.clamp(0.25, 4.0) as f32;
        if (core.rate() - rate).abs() > f32::EPSILON {
            let _ = core.set_rate(rate);
        }
        match core.state() {
            PlayState::Ready | PlayState::Paused if playing => {
                let _ = core.play();
            }
            PlayState::Playing if !playing => {
                let _ = core.pause();
            }
            _ => {}
        }
    }
    // Stopped sound is in step again when it starts.
    if !playing {
        running.audio_anchor = None;
    }
    let period = frame_period(core.info());
    let picture_time = core.presented().map(|p| p.pts_ns as f64 / 1e9);
    let drift = picture_time.map_or(0.0, |p| (source_time - p - period).max(0.0));
    let audio_drift = stream.as_ref().filter(|_| playing && !opening).and_then(|s| {
        if s.frames() == 0 || s.mix_rate() == 0 {
            return None;
        }
        let rate = f64::from(core.rate());
        if !MEDIA_FEED_RATES.contains(&core.rate()) {
            return None;
        }
        let (frames, at) = *running.audio_anchor.get_or_insert((s.frames(), source_time));
        let heard = at + s.frames().saturating_sub(frames) as f64 / f64::from(s.mix_rate()) * rate;
        Some(heard - source_time)
    });
    if !exact && !opening && playing && needs_resync(drift, audio_drift, source_time - running.last_resync) {
        running.last_resync = source_time;
        running.status.resyncs += 1;
        running.audio_anchor = None;
        core.reposition(t_ns);
    }
    let status = &mut running.status;
    status.state = core.state().name().to_string();
    status.source_time = source_time;
    status.frame_shown = core.frame_index();
    status.picture_time = picture_time;
    status.drift = drift;
    status.audio_drift = audio_drift;
    status.frames_dropped = core.stats().frames_dropped;
    state.set_external_time(t_ns as f64 / 1e9);
    issue
}

/// Gives the target back (a session that is gone or never opened).
fn release_target(world: &mut World, running: &mut RunningVideo) {
    let Some(media) = running.media else { return };
    if let Some(mut state) = world.get_mut::<VideoPlayerState>(media) {
        state.core_mut().close();
    }
    if let Some(mut registry) = world.get_resource_mut::<MediaAudioRegistry>() {
        registry.unregister(media.to_bits());
    }
    let restore = std::mem::replace(&mut running.restore, Restore::Spawned);
    match restore {
        Restore::Spawned => {
            if let Ok(entity) = world.get_entity_mut(media) {
                entity.despawn();
            }
        }
        Restore::Player(saved) => {
            if let Ok(mut entity) = world.get_entity_mut(media) {
                match saved {
                    // A fresh state: the player reopens what it played.
                    Some(player) => {
                        entity.insert((player, VideoPlayerState::default()));
                    }
                    None => {
                        entity.remove::<(VideoPlayer, VideoPlayerState)>();
                    }
                }
            }
        }
        Restore::Screen(mesh, material) => {
            if let Ok(mut entity) = world.get_entity_mut(mesh) {
                entity.remove::<(VideoMaterial, regia_media::frame_source::VideoMaterialBound)>();
                if let (Some(material), Some(mut renderer)) = (material, entity.get_mut::<MeshRenderer>()) {
                    renderer.material = material;
                }
            }
            if let Ok(entity) = world.get_entity_mut(media) {
                entity.despawn();
            }
        }
        Restore::Ui(node, image) => {
            if let Ok(mut entity) = world.get_entity_mut(node) {
                entity.remove::<(UiVideo, regia_ui::video::UiVideoShown)>();
                match image {
                    Some(image) => {
                        entity.insert(image);
                    }
                    None => {
                        entity.remove::<UiImage>();
                    }
                }
            }
            if let Ok(entity) = world.get_entity_mut(media) {
                entity.despawn();
            }
        }
    }
}

fn close_section(world: &mut World, mut running: RunningVideo) {
    release_target(world, &mut running);
}

/// A still of one source time, for the editor's section strip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoThumbnail {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// RGBA8 pixels, top row first.
    pub rgba: Vec<u8>,
    /// The index of the picture at the source's nominal rate.
    pub frame: u64,
}

/// Decodes the picture a section shows at `position` source seconds, on the calling thread (run it
/// off the frame: the keyframe before `position` and everything up to it decode here). Errors are
/// the media crate's.
pub fn video_thumbnail(files: &MediaFiles, video: &str, position: f64) -> Result<VideoThumbnail, MediaError> {
    let (file, _) = files.resolve(video)?;
    let reader = files.open(&file)?;
    let session = Session::open(
        reader,
        &SessionOptions {
            audio: regia_media::session::AudioChoice::None,
            ..SessionOptions::default()
        },
        None,
    )?;
    let info = session.info().clone();
    let mut core = PlayerCore::new();
    core.open(
        Box::new(session),
        info,
        ClockMode::Lockstep,
        LoopMode::Off,
        false,
        0.0,
    );
    core.show_at(media_time_ns(position, false, None), true, u32::MAX);
    if let Some(error) = core.last_error() {
        return Err(error.clone());
    }
    let frame = core.frame_index().unwrap_or(0);
    let picture = core
        .presented()
        .ok_or_else(|| MediaError::DecodeFailed("no decodable picture".into()))?;
    Ok(VideoThumbnail {
        width: picture.width,
        height: picture.height,
        rgba: picture_to_rgba8(picture),
        frame,
    })
}

#[cfg(test)]
mod tests;
