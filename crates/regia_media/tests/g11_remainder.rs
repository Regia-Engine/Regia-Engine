// SPDX-License-Identifier: MPL-2.0
//! Spec 50 remainder (ADR-2410): boot movies and `SkipVideo`, visibility `PauseDecode`, text cues on
//! the caption board, whole-file validation, the decode-pool limit and the thermal hook. Every
//! clip is generated in the test; everything runs in `Lockstep` and counts ticks.

use regia_app::prelude::*;
use regia_media::boot::{BOOT_MOVIE_LAYER, BootMovies, BootOutcome, SkipVideo, boot_movies_done};
use regia_media::mux::{PatternClip, pattern_index};
use regia_media::plugin::{MediaFiles, MediaSettings, VideoMessage, VisibilityPolicy, statuses};
use regia_media::{
    ClockMode, MediaPlugin, VideoEvent, VideoFullscreen, VideoPlayer, VideoPlayerState, VideoVisible,
};

fn app_with(files: MediaFiles) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .add_plugins(MediaPlugin);
    app
}

fn lockstep(source: &str) -> VideoPlayer {
    VideoPlayer {
        clock: ClockMode::Lockstep,
        ..VideoPlayer::new(source)
    }
}

fn shown(app: &mut App, e: Entity) -> Option<u32> {
    let mut state = app.world_mut().get_mut::<VideoPlayerState>(e)?;
    state.core_mut().presented().map(|p| pattern_index(&p.planes[0]))
}

fn frame_index(app: &mut App, e: Entity) -> Option<u64> {
    app.world_mut()
        .get_mut::<VideoPlayerState>(e)
        .and_then(|mut s| s.core_mut().frame_index())
}

fn messages(app: &App) -> Vec<(Entity, VideoEvent)> {
    app.world()
        .resource::<Messages<VideoMessage>>()
        .iter_current_update_messages()
        .map(|m| (m.entity, m.event.clone()))
        .collect()
}

/// Boot movies play in list order, each fullscreen above the UI; a skip before
/// `skippable_after` is refused, a later one skips; a movie that does not open is passed over.
#[test]
fn boot_sequence_plays_in_order_and_skip_works() {
    let mut files = MediaFiles::default();
    for name in ["a", "b", "c"] {
        files.insert(
            format!("movies/{name}.webm"),
            PatternClip::new(8, 8, 6, 60).to_webm(),
        );
    }
    let mut app = app_with(files);
    app.insert_resource(
        BootMovies::new([
            "movies/a.webm",
            "movies/missing.webm",
            "movies/b.webm",
            "movies/c.webm",
        ])
        .skippable_after(0.05)
        .with_clock(ClockMode::Lockstep),
    );
    assert!(!app.world().resource::<BootMovies>().is_done());

    app.update();
    let (index, first) = app.world().resource::<BootMovies>().current().expect("a plays");
    assert_eq!(index, 0);
    let full = app
        .world()
        .get::<VideoFullscreen>(first)
        .copied()
        .expect("fullscreen");
    assert_eq!(full.layer, BOOT_MOVIE_LAYER);
    assert!(full.layer >= 0, "boot movies draw above the UI");

    // Too early: refused, `a` keeps playing.
    app.world_mut().write_message(SkipVideo);
    app.update();
    let boot = app.world().resource::<BootMovies>();
    assert_eq!(boot.refused_skips(), 1);
    assert_eq!(boot.current().map(|c| c.0), Some(0));

    let mut skipped_b = false;
    for _ in 0..200 {
        if app.world().resource::<BootMovies>().is_done() {
            break;
        }
        let current = app.world().resource::<BootMovies>().current();
        // `b` is index 2; skip it once it has played past the delay.
        if let Some((2, e)) = current
            && !skipped_b
            && app
                .world()
                .get::<VideoPlayerState>(e)
                .is_some_and(|s| s.core().position() >= 0.05)
        {
            app.world_mut().write_message(SkipVideo);
            skipped_b = true;
        }
        app.update();
    }
    let boot = app.world().resource::<BootMovies>();
    assert!(boot.is_done());
    assert!(skipped_b);
    let got: Vec<(usize, &str, &str)> = boot
        .records()
        .iter()
        .map(|r| (r.index, r.source.as_str(), r.outcome.name()))
        .collect();
    assert_eq!(
        got,
        vec![
            (0, "movies/a.webm", "played"),
            (1, "movies/missing.webm", "failed"),
            (2, "movies/b.webm", "skipped"),
            (3, "movies/c.webm", "played"),
        ]
    );
    assert_eq!(
        boot.records()[1].outcome,
        BootOutcome::Failed("source_not_found".to_string())
    );
    app.update();
    // Every boot player is gone and the reveal condition holds.
    let left = app.world_mut().query::<&VideoPlayer>().iter(app.world()).count();
    assert_eq!(left, 0);
    assert!(
        app.world_mut()
            .run_system_cached(boot_movies_done)
            .expect("condition runs")
    );
}

/// `SkipVideo` also skips an ordinary skippable fullscreen player and leaves a non-skippable one.
#[test]
fn skip_video_message_skips_skippable_fullscreen_players() {
    let mut files = MediaFiles::default();
    files.insert("m.webm", PatternClip::new(8, 8, 30, 60).to_webm());
    let mut app = app_with(files);
    let yes = app
        .world_mut()
        .spawn((lockstep("m.webm"), VideoFullscreen::default()))
        .id();
    let no = app
        .world_mut()
        .spawn((
            lockstep("m.webm"),
            VideoFullscreen {
                skippable: false,
                ..VideoFullscreen::default()
            },
        ))
        .id();
    app.update();
    app.world_mut().write_message(SkipVideo);
    app.update();
    let ev = messages(&app);
    assert!(ev.contains(&(yes, VideoEvent::Ended { skipped: true })));
    assert!(!ev.iter().any(|(e, _)| *e == no));
}

/// A `PauseDecode` player hidden for 30 ticks presents nothing while hidden (frame index held, no
/// pictures counted) and, on the tick it shows again, presents the same frame as a player that was
/// never hidden; they stay in step after.
#[test]
fn visibility_pause_keeps_the_frame_index() {
    let mut files = MediaFiles::default();
    files.insert("wall.webm", PatternClip::new(8, 8, 90, 30).to_webm());
    let mut app = app_with(files);
    let seen = app.world_mut().spawn(lockstep("wall.webm")).id();
    let hidden = app
        .world_mut()
        .spawn((lockstep("wall.webm"), VideoVisible(true)))
        .id();
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(frame_index(&mut app, seen), frame_index(&mut app, hidden));
    app.world_mut().get_mut::<VideoVisible>(hidden).expect("vis").0 = false;
    app.update();
    let held = frame_index(&mut app, hidden);
    let presented = app
        .world_mut()
        .get_mut::<VideoPlayerState>(hidden)
        .expect("s")
        .core_mut()
        .stats()
        .frames_presented;
    for _ in 0..29 {
        app.update();
        assert_eq!(frame_index(&mut app, hidden), held, "no picture while hidden");
    }
    {
        let mut s = app.world_mut().get_mut::<VideoPlayerState>(hidden).expect("s");
        let core = s.core_mut();
        assert!(core.decode_paused());
        assert_eq!(core.paused_ticks(), 30);
        assert_eq!(core.stats().frames_presented, presented);
        assert!(
            core.position() > 0.6,
            "the clock kept running: {}",
            core.position()
        );
    }
    assert!(statuses(app.world_mut()).iter().any(|s| s.decode_paused));
    assert_ne!(frame_index(&mut app, seen), held, "the visible player moved on");
    app.world_mut().get_mut::<VideoVisible>(hidden).expect("vis").0 = true;
    for _ in 0..12 {
        app.update();
        assert_eq!(frame_index(&mut app, seen), frame_index(&mut app, hidden));
        assert_eq!(shown(&mut app, seen), shown(&mut app, hidden));
    }
    let s = app.world_mut().get_mut::<VideoPlayerState>(hidden).expect("s");
    assert_eq!(
        s.core().stats().frames_dropped,
        0,
        "resync discards are not drops"
    );
}

/// `PauseAll` freezes the clock too, and resumes from the same media time.
#[test]
fn pause_all_freezes_the_clock() {
    let mut files = MediaFiles::default();
    files.insert("wall.webm", PatternClip::new(8, 8, 90, 30).to_webm());
    let mut app = app_with(files);
    let e = app
        .world_mut()
        .spawn((
            VideoPlayer {
                visibility: VisibilityPolicy::PauseAll,
                ..lockstep("wall.webm")
            },
            VideoVisible(true),
        ))
        .id();
    for _ in 0..5 {
        app.update();
    }
    app.world_mut().get_mut::<VideoVisible>(e).expect("vis").0 = false;
    app.update();
    let pos = app
        .world()
        .get::<VideoPlayerState>(e)
        .expect("s")
        .core()
        .position();
    let index = frame_index(&mut app, e);
    for _ in 0..20 {
        app.update();
    }
    assert_eq!(
        app.world()
            .get::<VideoPlayerState>(e)
            .expect("s")
            .core()
            .position(),
        pos
    );
    app.world_mut().get_mut::<VideoVisible>(e).expect("vis").0 = true;
    app.update();
    assert_eq!(
        frame_index(&mut app, e),
        index,
        "the first tick after shows the held time"
    );
}

/// Text cues of a clip reach the caption board through the public caption messages: the board shows
/// the first cue while playback is inside it and the second once playback enters it.
#[test]
fn cues_reach_the_caption_board() {
    let clip = PatternClip::new(8, 8, 30, 30);
    let bytes = clip.to_webm_with_cues(&[
        (100_000_000, Some(200_000_000), "Hello from the clip"),
        (500_000_000, None, "Second line"),
    ]);
    let mut files = MediaFiles::default();
    files.insert("talk.webm", bytes);
    let mut app = app_with(files);
    app.add_plugins(regia_ui::captions::CaptionPlugin);
    let e = app.world_mut().spawn(lockstep("talk.webm")).id();
    let mut cue_events = Vec::new();
    let mut board_texts = Vec::new();
    for _ in 0..70 {
        app.update();
        for (who, ev) in messages(&app) {
            if who == e && matches!(ev, VideoEvent::TextCue { .. } | VideoEvent::TextCueEnd { .. }) {
                cue_events.push(ev);
            }
        }
        let snap = app
            .world()
            .resource::<regia_ui::captions::CaptionBoard>()
            .snapshot()
            .to_json(false)
            .to_string();
        board_texts.push(snap);
    }
    let kinds: Vec<&str> = cue_events.iter().map(VideoEvent::kind).collect();
    assert_eq!(
        kinds,
        vec!["text_cue", "text_cue_end", "text_cue", "text_cue_end"]
    );
    match &cue_events[0] {
        VideoEvent::TextCue {
            track,
            start,
            end,
            text,
        } => {
            assert_eq!(*track, 1, "the text track is the second track");
            assert!((start - 0.1).abs() < 1e-6);
            assert!(end.is_some_and(|e| (e - 0.3).abs() < 1e-6));
            assert_eq!(text, "Hello from the clip");
        }
        other => panic!("{other:?}"),
    }
    assert!(board_texts.iter().any(|t| t.contains("Hello from the clip")));
    assert!(board_texts.iter().any(|t| t.contains("Second line")));
    let routes = app.world().resource::<regia_media::plugin::VideoCaptionRoutes>();
    assert_eq!(routes.sent(), 2);
    assert_eq!(routes.live(), 0, "every cue was stopped");
}

/// The decode pool refuses the N+1th decoder with `limit_exceeded`; closing one frees its slot.
#[test]
fn decode_pool_refuses_the_extra_decoder() {
    let mut files = MediaFiles::default();
    files.insert("m.webm", PatternClip::new(8, 8, 30, 60).to_webm());
    let mut app = app_with(files);
    app.insert_resource(MediaSettings {
        max_decoders: 2,
        ..MediaSettings::default()
    });
    let players: Vec<Entity> = (0..3)
        .map(|_| app.world_mut().spawn(lockstep("m.webm")).id())
        .collect();
    app.update();
    let st = statuses(app.world_mut());
    let failed: Vec<&regia_media::plugin::PlayerStatus> = st.iter().filter(|s| s.state == "failed").collect();
    assert_eq!(failed.len(), 1, "{st:?}");
    assert_eq!(failed[0].last_error.as_deref(), Some("limit_exceeded"));
    assert_eq!(
        failed[0].player,
        players[2].to_bits(),
        "the last one in entity order"
    );
    let ev = messages(&app);
    assert!(ev.contains(&(
        players[2],
        VideoEvent::OpenFailed {
            code: "limit_exceeded".to_string()
        }
    )));
    // Close the first; the refused one reopens through the command layer.
    regia_media::plugin::with_core(app.world_mut(), players[0], |c| {
        c.close();
        Ok(())
    })
    .expect("close");
    let state =
        regia_media::plugin::command_open(app.world_mut(), players[2], lockstep("m.webm")).expect("open");
    assert_ne!(state, regia_media::PlayState::Failed);
}

/// Thermal hook: moderate halves the presented rate of world players, severe pauses
/// `PauseDecode` players; fullscreen players keep decoding at full rate.
#[test]
fn thermal_pressure_throttles_world_players_only() {
    use regia_platform::{ThermalState, ThermalStatus};
    let mut files = MediaFiles::default();
    files.insert("m.webm", PatternClip::new(8, 8, 120, 60).to_webm());
    let mut app = app_with(files);
    let world_screen = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Game,
            ..VideoPlayer::new("m.webm")
        })
        .id();
    let intro = app
        .world_mut()
        .spawn((lockstep("m.webm"), VideoFullscreen::default()))
        .id();
    app.insert_resource(ThermalState {
        status: ThermalStatus::Moderate,
        headroom: None,
    });
    app.update();
    let st = |app: &mut App, e: Entity| {
        statuses(app.world_mut())
            .into_iter()
            .find(|s| s.player == e.to_bits())
            .expect("status")
    };
    assert!(st(&mut app, world_screen).half_rate);
    assert!(!st(&mut app, intro).half_rate);
    app.insert_resource(ThermalState {
        status: ThermalStatus::Severe,
        headroom: None,
    });
    app.update();
    assert!(st(&mut app, world_screen).decode_paused);
    assert!(!st(&mut app, intro).decode_paused);
    app.insert_resource(ThermalState::default());
    app.update();
    assert!(!st(&mut app, world_screen).decode_paused);
    assert!(!st(&mut app, world_screen).half_rate);
}

/// Validation refuses a truncated file, a foreign container and a patented codec with their codes,
/// and reports the metadata of a good clip.
#[test]
fn validate_refuses_truncated_files_with_a_code() {
    let mut clip = PatternClip::new(16, 8, 12, 30);
    let good = clip.to_webm_with_cues(&[(0, Some(100_000_000), "hi")]);
    let report = regia_media::validate(std::io::Cursor::new(good.clone())).expect("valid");
    assert_eq!((report.width, report.height), (16, 8));
    assert_eq!(report.container, "webm");
    assert_eq!(report.tracks.len(), 2);
    assert_eq!(report.tracks[0].packets, 12);
    assert_eq!(report.tracks[1].kind, "text");
    assert!(report.frame_rate.is_some_and(|f| (f - 30.0).abs() < 0.01));
    for cut in [good.len() - 7, good.len() / 2, 60] {
        let err = regia_media::validate(std::io::Cursor::new(good[..cut].to_vec())).expect_err("truncated");
        assert_eq!(err.code(), "container_invalid", "cut at {cut}: {err}");
    }
    let mut mp4 = vec![0, 0, 0, 24];
    mp4.extend_from_slice(b"ftypisom");
    mp4.resize(64, 0);
    assert_eq!(
        regia_media::validate(std::io::Cursor::new(mp4))
            .expect_err("mp4")
            .code(),
        "container_unsupported"
    );
    clip.video_codec = "V_MPEG4/ISO/AVC".to_string();
    assert_eq!(
        regia_media::validate(std::io::Cursor::new(clip.to_webm()))
            .expect_err("avc")
            .code(),
        "codec_patented"
    );
}
