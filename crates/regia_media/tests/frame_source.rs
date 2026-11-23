// SPDX-License-Identifier: MPL-2.0
//! The renderer half of video playback (ADR-2044) on generated clips (no binary assets): the decode
//! ring stays inside its byte budget and drops stale pictures on seek; the published texture is
//! frame-exact after seeks and across loops; the fallback frame shows while there is no picture
//! and once decoding stalls; `UiVideo` shows the published picture and its transport widgets drive
//! the player; a `VideoMaterial` binds the player's texture. Every step is a fixed lockstep tick or
//! an explicit external time: no wall clock.

use std::io::Cursor;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_media::codec::DecodedPicture;
use regia_media::container::MediaInfo;
use regia_media::frame_source::{
    GpuVideoJobs, MediaFrameSource, VideoFrameSettings, VideoMaterial, VideoMaterialBound,
    VideoMaterialErrors, picture_planes,
};
use regia_media::mux::{PatternClip, pattern_index};
use regia_media::plugin::MediaFiles;
use regia_media::reference::{fnv1a64, picture_to_rgba8};
use regia_media::ring::{RingBudget, RingFeed};
use regia_media::session::{FeedPoll, FeedStats, FrameFeed, Session, SessionOptions};
use regia_media::{
    ClockMode, LoopMode, MediaPlugin, PlayState, VideoFramePlugin, VideoPlayer, VideoPlayerState,
};
use regia_render::video::yuv_to_rgba8;
use regia_render::{
    CpuMesh, LabMaterial, MeshRenderer, VideoControl, VideoFrameSource, VideoFrames, VideoSlot,
};
use regia_ui::interaction::{UiEvent, UiEventKind};
use regia_ui::render::{SharedUiRenderer, UiRenderer};
use regia_ui::video::{UiVideo, UiVideoAction, UiVideoControl, UiVideoPlugin, UiVideoShown, UiVideoStats};

fn session(clip: &PatternClip) -> Session {
    Session::open(Cursor::new(clip.to_webm()), &SessionOptions::default(), None).expect("open")
}

fn index_of(p: &DecodedPicture) -> u32 {
    pattern_index(&p.planes[0])
}

fn frame(poll: FeedPoll) -> DecodedPicture {
    match poll {
        FeedPoll::Frame(p) => p,
        other => panic!("expected a picture, got {other:?}"),
    }
}

/// The ring holds at most what its byte budget allows, delivers every picture in order, and its
/// peak never passes the budget.
#[test]
fn the_ring_stays_inside_its_byte_budget() {
    let clip = PatternClip::new(64, 32, 20, 30);
    let picture = 64 * 32 * 3 / 2;
    let budget = RingBudget {
        max_frames: 8,
        max_bytes: picture * 5 / 2,
    };
    let mut feed = RingFeed::spawn(session(&clip), budget).expect("spawn");
    let settled = feed.wait_settled();
    assert_eq!(settled.queued, 2, "two pictures fit in 2.5 pictures of budget");
    assert_eq!(settled.bytes, 2 * picture as u64);
    for k in 0..20 {
        let p = frame(feed.poll(true));
        assert_eq!(index_of(&p), k, "pictures arrive in order");
        let s = feed.wait_settled();
        assert!(s.bytes <= budget.max_bytes as u64, "step {k}: {s:?}");
    }
    assert!(matches!(feed.poll(true), FeedPoll::End));
    let s = feed.ring_stats();
    assert!(s.peak_bytes <= budget.max_bytes as u64, "{s:?}");
    assert_eq!(s.peak_bytes, 2 * picture as u64);
    assert!(s.full_waits > 0);
}

/// A budget smaller than one picture still decodes, one picture at a time.
#[test]
fn a_tiny_budget_decodes_one_picture_at_a_time() {
    let clip = PatternClip::new(32, 16, 6, 30);
    let mut feed = RingFeed::spawn(
        session(&clip),
        RingBudget {
            max_frames: 4,
            max_bytes: 1,
        },
    )
    .expect("spawn");
    for k in 0..6 {
        let s = feed.wait_settled();
        assert_eq!(s.queued, 1, "step {k}");
        assert_eq!(index_of(&frame(feed.poll(true))), k);
    }
    assert_eq!(feed.ring_stats().peak_bytes, 32 * 16 * 3 / 2);
}

/// A seek drops the queued pictures; the next picture is the cue point's.
#[test]
fn a_ring_seek_drops_stale_pictures() {
    let clip = PatternClip::new(32, 16, 30, 30);
    let mut feed = RingFeed::spawn(session(&clip), RingBudget::default()).expect("spawn");
    let s = feed.wait_settled();
    assert_eq!(s.queued, 4);
    feed.seek(clip.frame_pts_ns(12) as u64);
    // Cues every 5 frames: the cue at or before frame 12 is frame 10.
    assert_eq!(index_of(&frame(feed.poll(true))), 10);
    assert_eq!(index_of(&frame(feed.poll(true))), 11);
    assert!(feed.ring_stats().dropped_by_seek >= 4);
    let _: FeedStats = feed.stats();
}

fn app_with(clip: &PatternClip, ui: bool) -> App {
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("movies/clip.webm", clip.to_webm());
    files.insert(
        "movies/red.video.ron",
        r#"(format_version: 1, file: "movies/missing.webm", clear_color: (1.0, 0.0, 0.0, 1.0))"#
            .as_bytes()
            .to_vec(),
    );
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .add_plugins((MediaPlugin, VideoFramePlugin));
    // One 25 fps frame per lockstep update.
    app.world_mut()
        .resource_mut::<regia_media::plugin::MediaSettings>()
        .lockstep_dt_ns = 40_000_000;
    if ui {
        app.add_message::<UiEvent>()
            .insert_resource(SharedUiRenderer(UiRenderer::new_handle()))
            .add_plugins(UiVideoPlugin);
    }
    app
}

fn spawn_player(app: &mut App, loop_mode: LoopMode, autoplay: bool) -> Entity {
    app.world_mut()
        .spawn(VideoPlayer {
            autoplay,
            clock: ClockMode::Lockstep,
            loop_mode,
            ..VideoPlayer::new("movies/clip.webm")
        })
        .id()
}

fn source(app: &App) -> MediaFrameSource {
    app.world().resource::<MediaFrameSource>().clone()
}

/// The expected published bytes of pattern frame `k`.
fn expected_rgba(clip: &PatternClip, k: u32) -> Vec<u8> {
    let mut s = session(clip);
    let mut p = frame(s.next_picture());
    for _ in 0..k {
        p = frame(s.next_picture());
    }
    assert_eq!(index_of(&p), k);
    let mut out = Vec::new();
    assert!(yuv_to_rgba8(&picture_planes(&p), &mut out));
    // The fast path agrees with the double-precision reference within one step.
    let reference = picture_to_rgba8(&p);
    assert!(out.iter().zip(&reference).all(|(a, b)| a.abs_diff(*b) <= 1));
    out
}

fn published(app: &App, e: Entity) -> (Option<i64>, Arc<[u8]>) {
    let src = source(app);
    let info = src.frame(e.to_bits()).expect("published");
    let (_, _, _, rgba) = src.rgba(e.to_bits()).expect("rgba");
    (info.pts_ns, rgba)
}

/// Seeks through the frame source land on the exact frame: the published texture bytes equal the
/// conversion of decoding from 0 to that frame, for targets on and between cue points.
#[test]
fn seek_is_frame_exact_through_the_frame_source() {
    let clip = PatternClip::new(24, 12, 30, 25);
    let mut app = app_with(&clip, false);
    let e = spawn_player(&mut app, LoopMode::Off, false);
    app.update();
    let (pts, rgba) = published(&app, e);
    assert_eq!(pts, Some(clip.frame_pts_ns(0)));
    assert_eq!(&*rgba, expected_rgba(&clip, 0).as_slice(), "frame 0 after open");
    for k in [17u32, 3, 10, 29, 0, 22] {
        let t = clip.frame_pts_ns(k) as f64 / 1e9;
        source(&app)
            .control(e.to_bits(), VideoControl::Seek(t))
            .expect("queued");
        app.update();
        let (pts, rgba) = published(&app, e);
        assert_eq!(pts, Some(clip.frame_pts_ns(k)), "seek to frame {k}");
        assert_eq!(
            fnv1a64(&rgba),
            fnv1a64(&expected_rgba(&clip, k)),
            "frame {k} bytes"
        );
        let state = app
            .world_mut()
            .get_mut::<VideoPlayerState>(e)
            .expect("state")
            .core()
            .state();
        assert_eq!(state, PlayState::Paused, "a seek from ready lands paused");
    }
}

/// specs/50 § 8 test 5 through the frame source: a 10-frame clip at a 60 Hz lockstep shows
/// `0..9, 0..9, 0..4` over 25 updates, each texture frame-exact, with no fallback at the wrap.
#[test]
fn loop_is_frame_exact_through_the_frame_source() {
    let clip = PatternClip::new(16, 8, 10, 25);
    let mut app = app_with(&clip, false);
    let e = spawn_player(&mut app, LoopMode::Loop, true);
    let expected: Vec<Vec<u8>> = (0..10).map(|k| expected_rgba(&clip, k)).collect();
    let mut shown = Vec::new();
    let mut serials = Vec::new();
    for _ in 0..25 {
        app.update();
        let src = source(&app);
        let info = src.frame(e.to_bits()).expect("info");
        assert!(!info.fallback, "no fallback frame at the wrap");
        assert!(info.looping);
        let (_, rgba) = published(&app, e);
        let k = expected
            .iter()
            .position(|x| x.as_slice() == &*rgba)
            .expect("a pattern frame");
        shown.push(k as u32);
        serials.push(info.serial);
    }
    let want: Vec<u32> = (0..10).chain(0..10).chain(0..5).collect();
    assert_eq!(shown, want);
    assert!(
        serials.windows(2).all(|w| w[1] == w[0] + 1),
        "one upload per new picture"
    );
}

/// The SetLoop control applies to the session: `Off` ends after the pass in progress.
#[test]
fn set_loop_control_ends_at_the_next_wrap() {
    let clip = PatternClip::new(16, 8, 4, 25);
    let mut app = app_with(&clip, false);
    let e = spawn_player(&mut app, LoopMode::Loop, true);
    for _ in 0..6 {
        app.update();
    }
    source(&app)
        .control(e.to_bits(), VideoControl::SetLoop(false))
        .expect("queued");
    for _ in 0..10 {
        app.update();
    }
    let info = source(&app).frame(e.to_bits()).expect("info");
    assert_eq!(info.playback.name(), "ended");
    assert!(!info.looping);
    let player = app.world().get::<VideoPlayer>(e).expect("player");
    assert_eq!(
        player.loop_mode,
        LoopMode::Off,
        "the component follows the control"
    );
}

/// No picture: the fallback frame shows (black: a source that never opened has no clear color).
#[test]
fn a_failed_open_shows_the_clear_color() {
    let clip = PatternClip::new(16, 8, 4, 25);
    let mut app = app_with(&clip, false);
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            ..VideoPlayer::new("movies/red.video.ron")
        })
        .id();
    app.update();
    let src = source(&app);
    let info = src.frame(e.to_bits()).expect("info");
    assert!(info.fallback);
    assert_eq!(info.playback.name(), "failed");
    let (_, w, h, rgba) = src.rgba(e.to_bits()).expect("rgba");
    assert_eq!((w, h), (1, 1));
    assert_eq!(&*rgba, &[0, 0, 0, 255]);
    assert!(src.texture(e.to_bits()).is_some());
}

/// A feed that gives one picture and then nothing (a stalled decoder).
struct StallFeed(Option<DecodedPicture>);

impl FrameFeed for StallFeed {
    fn poll(&mut self, _block: bool) -> FeedPoll {
        match self.0.take() {
            Some(p) => FeedPoll::Frame(p),
            None => FeedPoll::Pending,
        }
    }

    fn seek(&mut self, _to_ns: u64) {}

    fn stats(&self) -> FeedStats {
        FeedStats::default()
    }
}

/// Decoding stalls: the last picture holds until the player has been `Buffering` for
/// `stall_ticks` ticks, then the clear color replaces it (at the picture's size).
#[test]
fn a_stall_shows_the_fallback_after_the_tick_limit() {
    let clip = PatternClip::new(16, 8, 30, 25);
    let mut app = app_with(&clip, false);
    app.world_mut().resource_mut::<VideoFrameSettings>().stall_ticks = 3;
    let info: MediaInfo = regia_media::probe(Cursor::new(clip.to_webm())).expect("probe");
    let first = frame(session(&clip).next_picture());
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            source: String::new(),
            clock: ClockMode::External,
            ..VideoPlayer::default()
        })
        .id();
    {
        let mut state = app.world_mut().get_mut::<VideoPlayerState>(e).expect("state");
        state.core_mut().open(
            Box::new(StallFeed(Some(first))),
            info,
            ClockMode::External,
            LoopMode::Off,
            true,
            0.0,
        );
    }
    let mut fallback_at = None;
    for step in 1..=20u32 {
        {
            let mut state = app.world_mut().get_mut::<VideoPlayerState>(e).expect("state");
            state.set_external_time(f64::from(step) * 0.05);
        }
        app.update();
        let info = source(&app).frame(e.to_bits()).expect("info");
        if info.fallback && fallback_at.is_none() {
            fallback_at = Some(step);
            let (_, w, h, rgba) = source(&app).rgba(e.to_bits()).expect("rgba");
            assert_eq!((w, h), (16, 8), "the fallback keeps the picture's size");
            assert!(rgba.chunks(4).all(|px| px == [0, 0, 0, 255]), "black clear color");
        }
        if fallback_at.is_none() && step > 1 {
            assert_eq!(info.pts_ns, Some(0), "the last picture holds before the limit");
        }
    }
    let at = fallback_at.expect("fallback shows");
    let status = source(&app)
        .statuses()
        .into_iter()
        .find(|s| s.player == e.to_bits())
        .expect("status");
    assert!(status.stall_ticks >= 3);
    // Buffering starts once more than 100 ms of media time starve (step 4 at 50 ms a step), and
    // the fallback follows three ticks of Buffering later.
    assert!((5..=8).contains(&at), "fallback at step {at}");
}

/// `UiVideo` shows the published picture; a toggle button and a seek slider drive the player.
#[test]
fn ui_video_shows_the_picture_and_drives_the_player() {
    let clip = PatternClip::new(16, 8, 20, 25);
    let mut app = app_with(&clip, true);
    let e = spawn_player(&mut app, LoopMode::Off, false);
    let node = app.world_mut().spawn(UiVideo::new(e.to_bits())).id();
    let toggle = app
        .world_mut()
        .spawn(UiVideoControl {
            video: node,
            action: UiVideoAction::Toggle,
        })
        .id();
    let seek = app
        .world_mut()
        .spawn(UiVideoControl {
            video: node,
            action: UiVideoAction::SeekSlider,
        })
        .id();
    app.update();
    app.update();
    let shown = *app.world().get::<UiVideoShown>(node).expect("shown");
    let info = source(&app).frame(e.to_bits()).expect("info");
    assert_eq!(shown.serial, info.serial);
    assert_eq!(shown.pts_ns, Some(0));
    assert!(shown.uploads >= 1);
    {
        let ui = app.world().resource::<SharedUiRenderer>().lock();
        assert_eq!(
            ui.image_size(shown.image),
            Some([16, 8]),
            "picture queued on the UI renderer"
        );
    }

    app.world_mut()
        .write_message(UiEvent::new(toggle, UiEventKind::Click));
    app.update();
    let state = app
        .world_mut()
        .get_mut::<VideoPlayerState>(e)
        .expect("state")
        .core()
        .state();
    assert_eq!(state, PlayState::Playing);
    for _ in 0..3 {
        app.update();
    }
    // Lockstep at 25 Hz on a 25 fps clip: one picture per update after the first tick.
    let shown = *app.world().get::<UiVideoShown>(node).expect("shown");
    assert_eq!(shown.pts_ns, Some(clip.frame_pts_ns(3)));

    app.world_mut().write_message(UiEvent::new(
        seek,
        UiEventKind::ValueChanged(regia_ui::interaction::UiValue::Number(0.5)),
    ));
    app.world_mut()
        .write_message(UiEvent::new(toggle, UiEventKind::Click));
    app.update();
    let shown = *app.world().get::<UiVideoShown>(node).expect("shown");
    assert_eq!(
        shown.pts_ns,
        Some(clip.frame_pts_ns(10)),
        "seek to half of 20 frames"
    );
    let state = app
        .world_mut()
        .get_mut::<VideoPlayerState>(e)
        .expect("state")
        .core()
        .state();
    assert_eq!(state, PlayState::Paused);
    assert_eq!(app.world().resource::<UiVideoStats>().sent, 3);
}

/// A `VideoMaterial` binds the player's texture into the mesh material's slot; without a mesh the
/// binding reports `video_material_missing_video_slot`.
#[test]
fn video_material_binds_the_player_texture() {
    let clip = PatternClip::new(16, 8, 4, 25);
    let mut app = app_with(&clip, false);
    let e = spawn_player(&mut app, LoopMode::Off, false);
    let mesh = Arc::new(CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    });
    let screen = app
        .world_mut()
        .spawn((
            MeshRenderer::new(mesh, Arc::new(LabMaterial::default())),
            VideoMaterial {
                player: e,
                slot: VideoSlot::Emissive,
            },
        ))
        .id();
    let bare = app
        .world_mut()
        .spawn(VideoMaterial {
            player: e,
            slot: VideoSlot::Albedo,
        })
        .id();
    app.update();
    app.update();
    let texture = source(&app).texture(e.to_bits()).expect("texture");
    let pending = format!("{texture:?}");
    assert!(
        pending.contains("pending_bytes: Some("),
        "a VideoMaterial copies the picture into the texture, got {pending}"
    );
    let renderer = app.world().get::<MeshRenderer>(screen).expect("mesh");
    assert!(regia_render::video::binds_video(
        &renderer.material,
        &texture,
        VideoSlot::Emissive
    ));
    assert!(app.world().get::<VideoMaterialBound>(screen).is_some());
    let errors = app.world().resource::<VideoMaterialErrors>();
    assert_eq!(
        errors.0.get(&bare.to_bits()).map(String::as_str),
        Some("video_material_missing_video_slot")
    );
    // The same texture stays bound while the picture changes (no material churn).
    let before = Arc::as_ptr(&renderer.material);
    source(&app)
        .control(e.to_bits(), VideoControl::Play)
        .expect("play");
    for _ in 0..3 {
        app.update();
    }
    let renderer = app.world().get::<MeshRenderer>(screen).expect("mesh");
    assert_eq!(Arc::as_ptr(&renderer.material), before);
}

/// The plugin turns the decode ring on for players it opens, and the GPU path queues pictures for
/// the convert pass only when asked.
#[test]
fn the_plugin_installs_the_ring_and_the_source() {
    let clip = PatternClip::new(16, 8, 8, 25);
    let mut app = app_with(&clip, false);
    assert!(app.world().resource::<VideoFrames>().source().is_some());
    let settings = app.world().resource::<regia_media::plugin::MediaSettings>();
    assert!(settings.ring_budget.is_some());
    app.insert_resource(GpuVideoJobs::default());
    app.world_mut().resource_mut::<VideoFrameSettings>().gpu_convert = true;
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Game,
            ..VideoPlayer::new("movies/clip.webm")
        })
        .id();
    // A worker-path player: open, then wait for the first picture through the ring (polls the
    // condition; the decode thread needs microseconds per 16x8 picture).
    for _ in 0..100_000 {
        app.update();
        if source(&app).frame(e.to_bits()).is_some_and(|i| !i.fallback) {
            break;
        }
        std::thread::yield_now();
    }
    let status = source(&app)
        .statuses()
        .into_iter()
        .find(|s| s.player == e.to_bits())
        .expect("status");
    let ring = status.ring.expect("the session decodes into a ring");
    assert!(ring.peak_bytes <= ring.budget_bytes);
    assert!(status.gpu_submits >= 1, "{status:?}");
    assert!(app.world().resource::<GpuVideoJobs>().0.stats().pending >= 1);
}

/// A `UiVideo` player on the audio clock follows the frames the audio output consumed: one 25 fps
/// picture per 1920 frames at 48 kHz, and the picture freezes while the audio clock stalls.
#[test]
fn ui_video_on_the_audio_clock_follows_and_stalls_with_it() {
    let clip = PatternClip::new(16, 8, 20, 25);
    let mut app = app_with(&clip, true);
    let clock = regia_audio::media_feed::AudioClock::new(48_000);
    app.insert_resource(clock.clone());
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Audio,
            audio_track: None,
            ..VideoPlayer::new("movies/clip.webm")
        })
        .id();
    let node = app.world_mut().spawn(UiVideo::new(e.to_bits())).id();
    // Opening on the worker path: wait (polling the condition) for the first picture.
    for _ in 0..100_000 {
        app.update();
        if app
            .world()
            .get::<UiVideoShown>(node)
            .is_some_and(|s| s.pts_ns.is_some())
        {
            break;
        }
        std::thread::yield_now();
    }
    // One playing tick anchors the media time to the audio clock's current frame count.
    app.update();
    let info = source(&app).frame(e.to_bits()).expect("info");
    assert!(info.audio_clock, "the player follows the audio clock");
    assert_eq!(info.playback.name(), "playing");
    let pts = |app: &App| app.world().get::<UiVideoShown>(node).and_then(|s| s.pts_ns);
    assert_eq!(pts(&app), Some(0));
    // Advance the audio clock by exactly one picture period per step; wait for each picture.
    for k in 1..=5u32 {
        clock.advance(1920);
        for _ in 0..100_000 {
            app.update();
            if pts(&app) == Some(clip.frame_pts_ns(k)) {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(
            pts(&app),
            Some(clip.frame_pts_ns(k)),
            "picture {k} follows the audio clock"
        );
    }
    // The audio clock stalls: the picture holds (game time keeps running).
    for _ in 0..50 {
        app.update();
    }
    assert_eq!(
        pts(&app),
        Some(clip.frame_pts_ns(5)),
        "a stalled audio clock freezes the picture"
    );
}

/// A player with `VideoFullscreen` is queued for the fullscreen pass with its picture; a closed
/// player leaves the queue.
#[test]
fn fullscreen_players_fill_the_fullscreen_queue() {
    use regia_media::frame_source::FullscreenVideos;
    use regia_media::present::{Fit, VideoFullscreen};
    let clip = PatternClip::new(16, 8, 10, 25);
    let mut app = app_with(&clip, false);
    app.insert_resource(FullscreenVideos::default());
    let e = spawn_player(&mut app, LoopMode::Off, true);
    app.world_mut().entity_mut(e).insert(VideoFullscreen {
        fit: Fit::Fill,
        layer: -1,
        skippable: true,
    });
    let other = spawn_player(&mut app, LoopMode::Off, true);
    app.update();
    let queue = app.world().resource::<FullscreenVideos>().0.clone();
    let videos = queue.videos();
    assert_eq!(videos.len(), 1, "only the fullscreen player");
    assert_eq!(videos[0].player, e.to_bits());
    assert_eq!(videos[0].fit, regia_render::FullscreenFit::Fill);
    assert_eq!((videos[0].width, videos[0].height, videos[0].layer), (16, 8, -1));
    assert_eq!(&*videos[0].rgba, expected_rgba(&clip, 0).as_slice());
    let _ = other;
    {
        let mut state = app.world_mut().get_mut::<VideoPlayerState>(e).expect("state");
        assert!(state.core_mut().skip());
    }
    app.update();
    assert!(queue.videos().is_empty(), "a skipped player leaves the screen");
}

/// Two same-size pictures share one RGBA allocation, and a fullscreen player with no
/// `VideoMaterial` does not store a second copy in `DynamicTexture`.
#[test]
fn same_size_publish_reuses_rgba_and_skips_an_unbound_texture() {
    use regia_media::frame_source::FullscreenVideos;
    use regia_media::present::{Fit, VideoFullscreen};
    let clip = PatternClip::new(16, 8, 4, 25);
    let mut app = app_with(&clip, false);
    app.insert_resource(FullscreenVideos::default());
    let e = spawn_player(&mut app, LoopMode::Off, true);
    app.world_mut().entity_mut(e).insert(VideoFullscreen {
        fit: Fit::Letterbox,
        layer: -1,
        skippable: true,
    });
    app.update();
    let (_, rgba0) = published(&app, e);
    let ptr0 = rgba0.as_ptr() as usize;
    assert_eq!(&*rgba0, expected_rgba(&clip, 0).as_slice(), "frame 0");
    drop(rgba0);
    app.update();
    let (_, rgba1) = published(&app, e);
    let ptr1 = rgba1.as_ptr() as usize;
    assert_eq!(&*rgba1, expected_rgba(&clip, 1).as_slice(), "frame 1");
    assert_eq!(ptr0, ptr1, "second same-size publish reuses the RGBA allocation");
    let texture = source(&app).texture(e.to_bits()).expect("texture handle");
    let pending = format!("{texture:?}");
    assert!(
        pending.contains("pending_bytes: None"),
        "no VideoMaterial: DynamicTexture pending must stay empty, got {pending}"
    );
}

/// The GPU convert queue shares the presented picture's plane allocations.
///
/// Lockstep keeps the picture only in `presented` (no decode ring). One `app.update` opens
/// the clip and publishes frame 0, so each plane's strong count is the picture plus the queue.
#[test]
fn gpu_convert_shares_decoder_plane_allocations() {
    let clip = PatternClip::new(16, 8, 2, 25);
    let mut app = app_with(&clip, false);
    app.insert_resource(GpuVideoJobs::default());
    app.world_mut().resource_mut::<VideoFrameSettings>().gpu_convert = true;
    let e = spawn_player(&mut app, LoopMode::Off, false);
    app.update();
    let counts = {
        let state = app.world().get::<VideoPlayerState>(e).expect("state");
        let core = state.core();
        let picture = core.presented().expect("presented");
        picture.planes.each_ref().map(Arc::strong_count)
    };
    assert_eq!(
        counts,
        [2, 2, 2],
        "the convert queue must share the decoder plane allocations, got {counts:?}"
    );
    assert!(
        app.world().resource::<GpuVideoJobs>().0.stats().pending >= 1,
        "the picture was queued"
    );
}

/// `Skip` closes a skippable player with `Ended { skipped: true }`; a fullscreen player marked not
/// skippable refuses with `video_not_skippable` and keeps playing.
#[test]
fn skip_honors_the_skippable_flag() {
    use regia_media::plugin::VideoMessage;
    use regia_media::present::{Fit, VideoFullscreen};
    let clip = PatternClip::new(16, 8, 10, 25);
    let mut app = app_with(&clip, false);
    let locked = spawn_player(&mut app, LoopMode::Off, true);
    app.world_mut().entity_mut(locked).insert(VideoFullscreen {
        fit: Fit::Letterbox,
        layer: 0,
        skippable: false,
    });
    let free = spawn_player(&mut app, LoopMode::Off, true);
    app.update();
    let src = source(&app);
    src.control(locked.to_bits(), VideoControl::Skip).expect("queued");
    src.control(free.to_bits(), VideoControl::Skip).expect("queued");
    app.update();
    let skipped: Vec<Entity> = app
        .world()
        .resource::<Messages<VideoMessage>>()
        .iter_current_update_messages()
        .filter(|m| m.event == regia_media::VideoEvent::Ended { skipped: true })
        .map(|m| m.entity)
        .collect();
    assert_eq!(skipped, vec![free]);
    let status = |e: Entity| {
        source(&app)
            .statuses()
            .into_iter()
            .find(|s| s.player == e.to_bits())
            .expect("status")
    };
    assert_eq!(status(free).state, "closed");
    assert_eq!(status(locked).state, "playing");
    assert_eq!(
        status(locked).last_control_error.as_deref(),
        Some("video_not_skippable")
    );
}

/// One raw I420 frame whose coded size and Matroska display size differ.
fn anamorphic_webm(coded: (u32, u32), display: (u32, u32)) -> Vec<u8> {
    use regia_media::codec::RAW_VIDEO;
    use regia_media::ebml::{uint_bytes, write_element};
    use regia_media::mux::pattern_frame;

    fn el(out: &mut Vec<u8>, eid: u32, body: &[u8]) {
        write_element(out, eid, body);
    }
    fn uel(out: &mut Vec<u8>, eid: u32, value: u64) {
        el(out, eid, &uint_bytes(value));
    }

    const INFO: u32 = 0x1549_A966;
    const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
    const DURATION: u32 = 0x4489;
    const TRACKS: u32 = 0x1654_AE6B;
    const TRACK_ENTRY: u32 = 0xAE;
    const TRACK_NUMBER: u32 = 0xD7;
    const TRACK_TYPE: u32 = 0x83;
    const CODEC_ID: u32 = 0x86;
    const VIDEO: u32 = 0xE0;
    const PIXEL_WIDTH: u32 = 0xB0;
    const PIXEL_HEIGHT: u32 = 0xBA;
    const DISPLAY_WIDTH: u32 = 0x54B0;
    const DISPLAY_HEIGHT: u32 = 0x54BA;
    const FOURCC: u32 = 0x2E_B524;
    const CLUSTER: u32 = 0x1F43_B675;
    const TIMESTAMP: u32 = 0xE7;
    const SIMPLE_BLOCK: u32 = 0xA3;
    const EBML: u32 = 0x1A45_DFA3;
    const DOC_TYPE: u32 = 0x4282;
    const SEGMENT: u32 = 0x1853_8067;

    let mut video = Vec::new();
    uel(&mut video, PIXEL_WIDTH, u64::from(coded.0));
    uel(&mut video, PIXEL_HEIGHT, u64::from(coded.1));
    uel(&mut video, DISPLAY_WIDTH, u64::from(display.0));
    uel(&mut video, DISPLAY_HEIGHT, u64::from(display.1));
    el(&mut video, FOURCC, b"I420");
    let mut entry = Vec::new();
    uel(&mut entry, TRACK_NUMBER, 1);
    uel(&mut entry, TRACK_TYPE, 1);
    el(&mut entry, CODEC_ID, RAW_VIDEO.as_bytes());
    el(&mut entry, VIDEO, &video);
    let mut tracks = Vec::new();
    el(&mut tracks, TRACK_ENTRY, &entry);
    let mut tracks_el = Vec::new();
    el(&mut tracks_el, TRACKS, &tracks);

    let mut info = Vec::new();
    uel(&mut info, TIMESTAMP_SCALE, 1_000_000);
    let units = 40_000_000.0f64 / 1_000_000.0;
    el(&mut info, DURATION, &units.to_bits().to_be_bytes());
    let mut info_el = Vec::new();
    el(&mut info_el, INFO, &info);

    let mut block = Vec::new();
    block.push(0x81);
    block.extend_from_slice(&0i16.to_be_bytes());
    block.push(0x80);
    block.extend(pattern_frame(coded.0, coded.1, 0));
    let mut cluster = Vec::new();
    uel(&mut cluster, TIMESTAMP, 0);
    el(&mut cluster, SIMPLE_BLOCK, &block);
    let mut cluster_el = Vec::new();
    el(&mut cluster_el, CLUSTER, &cluster);

    let mut body = Vec::new();
    body.extend(info_el);
    body.extend(tracks_el);
    body.extend(cluster_el);

    let mut header = Vec::new();
    uel(&mut header, 0x4286, 1);
    uel(&mut header, 0x42F7, 1);
    uel(&mut header, 0x42F2, 4);
    uel(&mut header, 0x42F3, 8);
    el(&mut header, DOC_TYPE, b"webm");
    uel(&mut header, 0x4287, 4);
    uel(&mut header, 0x4285, 2);
    let mut out = Vec::new();
    el(&mut out, EBML, &header);
    el(&mut out, SEGMENT, &body);
    out
}

/// Finding 2: a 1440×1080 picture with display size 1920×1080 is published at 16:9.
/// The texture stays the coded size.
#[test]
fn anamorphic_display_size_sets_the_published_aspect() {
    let bytes = anamorphic_webm((1440, 1080), (1920, 1080));
    let probed = regia_media::probe(Cursor::new(bytes.clone())).expect("probe");
    let video = probed
        .video_track()
        .and_then(|track| track.video.clone())
        .expect("video");
    assert_eq!(
        (
            video.width,
            video.height,
            video.display_width,
            video.display_height
        ),
        (1440, 1080, 1920, 1080)
    );
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("movies/clip.webm", bytes);
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .add_plugins((MediaPlugin, VideoFramePlugin));
    app.world_mut()
        .resource_mut::<regia_media::plugin::MediaSettings>()
        .lockstep_dt_ns = 40_000_000;
    let entity = spawn_player(&mut app, LoopMode::Off, false);
    app.update();
    let info = source(&app).frame(entity.to_bits()).expect("published");
    assert_eq!((info.width, info.height), (1440, 1080));
    let want = 1920.0f32 / 1080.0;
    assert!(
        (info.aspect - want).abs() < 1.0e-4,
        "display aspect {want}, published {}",
        info.aspect
    );
}
