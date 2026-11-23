// SPDX-License-Identifier: MPL-2.0
//! `MediaPlugin` end to end in an app: a `VideoPlayer` on an entity opens from `MediaFiles`, plays in
//! `Lockstep`, emits `VideoMessage`s in order, and the agent command layer reads it.

use regia_app::prelude::*;
use regia_media::mux::{PatternClip, pattern_index};
use regia_media::plugin::{
    MediaFiles, VideoMessage, command_open, command_seek, find_player, frame_hash, statuses,
};
use regia_media::{
    ClockMode, LoopMode, MediaPlugin, PlayState, SeekMode, VideoEvent, VideoPlayer, VideoPlayerState,
};

fn app_with_clip(frames: u32) -> App {
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert(
        "movies/pattern.webm",
        PatternClip::new(16, 8, frames, 60).to_webm(),
    );
    files.insert(
        "movies/pattern.video.ron",
        r#"(format_version: 1, file: "movies/pattern.webm", clear_color: (1.0, 0.0, 0.0, 1.0))"#
            .as_bytes()
            .to_vec(),
    );
    files.insert("movies/clip.mp4", vec![0u8; 16]);
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .add_plugins(MediaPlugin);
    app
}

fn events(app: &App) -> Vec<(Entity, VideoEvent)> {
    let messages = app.world().resource::<Messages<VideoMessage>>();
    messages
        .iter_current_update_messages()
        .map(|m| (m.entity, m.event.clone()))
        .collect()
}

fn shown(app: &mut App, e: Entity) -> u32 {
    let mut state = app.world_mut().get_mut::<VideoPlayerState>(e).expect("state");
    pattern_index(&state.core_mut().presented().expect("presented").planes[0])
}

#[test]
fn lockstep_player_plays_to_the_end_with_ordered_messages() {
    let mut app = app_with_clip(5);
    let e = app
        .world_mut()
        .spawn((
            Name::new("wall"),
            VideoPlayer {
                clock: ClockMode::Lockstep,
                ..VideoPlayer::new("movies/pattern.video.ron")
            },
        ))
        .id();
    app.update();
    let first = events(&app);
    assert_eq!(first, vec![(e, VideoEvent::Opened)]);
    assert_eq!(
        shown(&mut app, e),
        0,
        "the first lockstep tick presents media time 0"
    );
    let mut seen = Vec::new();
    for _ in 0..6 {
        app.update();
        seen.extend(events(&app));
    }
    assert!(seen.contains(&(e, VideoEvent::Ended { skipped: false })));
    let status = statuses(app.world_mut());
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].state, "ended");
    assert_eq!(find_player(app.world_mut(), "wall").ok(), Some(e));
    assert_eq!(
        find_player(app.world_mut(), &e.to_bits().to_string()).ok(),
        Some(e)
    );
    assert_eq!(
        find_player(app.world_mut(), "nobody").map_err(|x| x.code()),
        Err("player_not_found")
    );
}

#[test]
fn closed_frame_is_the_clear_color_and_hashes_are_stable() {
    let mut app = app_with_clip(8);
    let e = app.world_mut().spawn(VideoPlayer::default()).id();
    app.update();
    let (hash_closed, w, h, _) = frame_hash(app.world_mut(), e, None).expect("hash");
    assert_eq!((w, h), (1, 1));
    assert_eq!(
        hash_closed,
        regia_media::reference::fnv1a64(&[0, 0, 0, 255]),
        "default clear color is black"
    );

    let player = VideoPlayer {
        clock: ClockMode::Lockstep,
        autoplay: false,
        loop_mode: LoopMode::Loop,
        ..VideoPlayer::new("movies/pattern.video.ron")
    };
    assert_eq!(command_open(app.world_mut(), e, player), Ok(PlayState::Ready));
    command_seek(app.world_mut(), e, None, Some(5), SeekMode::Accurate).expect("seek");
    let (a, w, h, _) = frame_hash(app.world_mut(), e, None).expect("hash");
    assert_eq!((w, h), (16, 8));
    assert_eq!(shown(&mut app, e), 5);
    command_seek(app.world_mut(), e, None, Some(5), SeekMode::Accurate).expect("seek again");
    let (b, _, _, _) = frame_hash(app.world_mut(), e, None).expect("hash");
    assert_eq!(a, b);
    app.world_mut()
        .get_mut::<VideoPlayerState>(e)
        .expect("state")
        .core_mut()
        .close();
    let (closed, _, _, _) = frame_hash(app.world_mut(), e, None).expect("hash");
    assert_eq!(
        closed,
        regia_media::reference::fnv1a64(&[255, 0, 0, 255]),
        "the .video.ron clear color"
    );
}

#[test]
fn refused_sources_fail_with_codes() {
    let mut app = app_with_clip(4);
    let mp4 = app.world_mut().spawn(VideoPlayer::new("movies/clip.mp4")).id();
    let missing = app.world_mut().spawn(VideoPlayer::new("movies/none.webm")).id();
    let escape = app.world_mut().spawn(VideoPlayer::new("../secret.webm")).id();
    app.update();
    let codes: Vec<(Entity, VideoEvent)> = events(&app);
    let code_of = |e: Entity| codes.iter().find(|(x, _)| *x == e).map(|(_, ev)| ev.clone());
    assert_eq!(
        code_of(mp4),
        Some(VideoEvent::OpenFailed {
            code: "container_unsupported".into()
        })
    );
    assert_eq!(
        code_of(missing),
        Some(VideoEvent::OpenFailed {
            code: "source_not_found".into()
        })
    );
    assert_eq!(
        code_of(escape),
        Some(VideoEvent::OpenFailed {
            code: "source_not_found".into()
        })
    );
    // Messages are sorted by entity bits.
    let order: Vec<u64> = codes.iter().map(|(e, _)| e.to_bits()).collect();
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(order, sorted);
}

#[test]
fn ninth_player_is_limit_exceeded() {
    let mut app = app_with_clip(4);
    let players: Vec<Entity> = (0..9)
        .map(|_| {
            app.world_mut()
                .spawn(VideoPlayer {
                    clock: ClockMode::Lockstep,
                    autoplay: false,
                    ..VideoPlayer::new("movies/pattern.webm")
                })
                .id()
        })
        .collect();
    app.update();
    let failed: Vec<Entity> = events(&app)
        .into_iter()
        .filter(|(_, ev)| {
            *ev == VideoEvent::OpenFailed {
                code: "limit_exceeded".into(),
            }
        })
        .map(|(e, _)| e)
        .collect();
    assert_eq!(failed.len(), 1);
    assert!(players.contains(&failed[0]));
}

/// Media audit 4: a symlink inside the root cannot read outside it; one that stays inside works.
#[cfg(unix)]
#[test]
fn a_symlink_cannot_leave_the_media_root() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir().join(format!(
        "regia_media_links_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let root = base.join("root");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(base.join("secret.txt"), "secret").expect("outside");
    std::fs::write(root.join("clip.txt"), "inside").expect("inside");
    std::os::unix::fs::symlink(base.join("secret.txt"), root.join("out.txt")).expect("link");
    std::os::unix::fs::symlink(root.join("clip.txt"), root.join("in.txt")).expect("link");
    let files = MediaFiles::with_root(&root);
    assert_eq!(files.read_text("clip.txt").expect("plain file"), "inside");
    assert_eq!(
        files.read_text("in.txt").expect("a link inside the root"),
        "inside"
    );
    let refused = files.read_text("out.txt").expect_err("a link out of the root");
    assert_eq!(refused.code(), "source_not_found");
    std::fs::remove_dir_all(&base).expect("cleanup");
}

/// Media audit G 4: on Windows the verified canonicalized path (the `\\?\` extended-length form)
/// is the one opened, so a project-relative media path whose absolute form passes MAX_PATH opens.
/// (Only meaningful on Windows: everywhere else `canonicalize` does not change the path form and
/// the classic length limit does not apply.)
#[cfg(windows)]
#[test]
fn windows_long_media_path_opens() {
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir().join(format!(
        "regia_media_long_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let root = base.join("project");
    std::fs::create_dir_all(&root).expect("root");
    // Build the deep chain through the canonicalized (extended-length) parent: plain std calls
    // would hit the same MAX_PATH limit the test exercises.
    let deep = "d".repeat(120);
    let file_name = format!("{}.webm", "c".repeat(120));
    let mut dir = std::fs::canonicalize(&root).expect("canonical root");
    for part in ["movies", deep.as_str()] {
        dir.push(part);
        std::fs::create_dir(&dir).expect("deep mkdir");
    }
    std::fs::write(dir.join(&file_name), PatternClip::new(16, 8, 4, 30).to_webm()).expect("clip");
    let rel = format!("movies/{deep}/{file_name}");
    let absolute = std::fs::canonicalize(dir.join(&file_name)).expect("file");
    assert!(
        absolute.as_os_str().len() > 260,
        "the absolute path is {} chars, past MAX_PATH",
        absolute.as_os_str().len()
    );
    let files = MediaFiles::with_root(&root);
    let mut reader = files
        .open(&rel)
        .expect("the canonicalized path opens past MAX_PATH");
    let mut head = [0u8; 4];
    reader.read_exact(&mut head).expect("read");
    assert_eq!(&head, b"\x1a\x45\xdf\xa3", "EBML magic");
    // Cleanup goes through the canonicalized (extended-length on Windows) form: the plain
    // `base` path would itself exceed MAX_PATH deep inside the tree (fixrev review B finding 3).
    let canonical_base = std::fs::canonicalize(&base).expect("canonical base");
    std::fs::remove_dir_all(&canonical_base).expect("cleanup");
}

/// A deep path still opens through the root on every OS (the confinement check passes and the
/// returned reader is the verified one); the >260-char Windows case is `windows_long_media_path_opens`.
#[test]
fn deep_media_path_opens_portably() {
    use std::io::Read;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "regia_media_deep_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let rel = Path::new("movies").join("campaign").join("deep.webm");
    std::fs::create_dir_all(root.join(rel.parent().expect("dir"))).expect("dirs");
    std::fs::write(root.join(&rel), PatternClip::new(16, 8, 4, 30).to_webm()).expect("clip");
    let mut reader = MediaFiles::with_root(&root)
        .open(rel.to_str().expect("utf8"))
        .expect("opens");
    let mut head = [0u8; 4];
    reader.read_exact(&mut head).expect("read");
    assert_eq!(&head, b"\x1a\x45\xdf\xa3", "EBML magic");
    std::fs::remove_dir_all(&root).expect("cleanup");
}

#[test]
fn changing_the_source_keeps_the_new_audio_feed_registered() {
    use regia_audio::media_feed::MediaAudioRegistry;
    use regia_media::mux::ToneAudio;
    let mut app = app_with_clip(8);
    for name in ["movies/a.webm", "movies/b.webm"] {
        let mut clip = PatternClip::new(16, 8, 8, 30);
        clip.audio = Some(ToneAudio {
            rate: 48_000,
            channels: 2,
            freq_hz: 440.0,
            amplitude: 0.5,
        });
        app.world_mut()
            .resource_mut::<MediaFiles>()
            .insert(name, clip.to_webm());
    }
    app.init_resource::<MediaAudioRegistry>();
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            ..VideoPlayer::new("movies/a.webm")
        })
        .id();
    app.update();
    assert_eq!(app.world().resource::<MediaAudioRegistry>().feeds().len(), 1);
    app.world_mut().get_mut::<VideoPlayer>(e).expect("player").source = "movies/b.webm".into();
    app.update();
    assert!(
        events(&app).iter().any(|(_, ev)| *ev == VideoEvent::Closed),
        "the reopen closed the old session"
    );
    assert_eq!(
        app.world().resource::<MediaAudioRegistry>().feeds().len(),
        1,
        "the new session's feed survives the old session's Closed"
    );
}

/// Media audit G finding 1 (amended ADR-5441): on Android the umbrella extracts the packaged
/// media files listed by the package-time `media_index.ron` out of the APK next to the paks, at
/// the layout the sources name — `<internal>/content/movies/...`, so a documented
/// `movies/intro.webm` source (specs/50 § 4) resolves with `MediaFiles` rooted at the content
/// dir. A player on a game that ships its movie as an APK asset opens and plays from that mount.
/// Simulated here with an extracted-content dir in a temp folder.
#[test]
fn android_packaged_media_opens_from_the_content_mount() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let content = std::env::temp_dir().join(format!(
        "regia_media_mount_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // The documented project-path form (no `assets/` prefix): the extraction preserves the
    // APK-assets-relative layout into the content dir.
    let rel = "movies/pattern.webm";
    let clip_path = content.join(rel);
    std::fs::create_dir_all(clip_path.parent().expect("dir")).expect("dirs");
    std::fs::write(&clip_path, PatternClip::new(16, 8, 5, 60).to_webm()).expect("clip");

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(MediaFiles::with_root(&content))
        .add_plugins(MediaPlugin);
    let e = app
        .world_mut()
        .spawn((
            Name::new("packaged"),
            VideoPlayer {
                clock: ClockMode::Lockstep,
                ..VideoPlayer::new(rel)
            },
        ))
        .id();
    app.update();
    assert_eq!(
        events(&app),
        vec![(e, VideoEvent::Opened)],
        "the APK-shipped clip opens from the extracted content mount"
    );
    assert_eq!(
        shown(&mut app, e),
        0,
        "the first lockstep tick presents media time 0"
    );
    // Decode is asynchronous: step (a bounded, fixed count — never wall clock) until the last
    // picture presents.
    for _ in 0..8 {
        if shown(&mut app, e) == 4 {
            break;
        }
        app.update();
    }
    assert_eq!(
        shown(&mut app, e),
        4,
        "the clip plays through to its last picture"
    );
    let status = statuses(app.world_mut());
    assert_eq!(status[0].last_error, None);
    std::fs::remove_dir_all(&content).expect("cleanup");
}

fn collect(app: &mut App, updates: usize) -> Vec<VideoEvent> {
    let mut all = Vec::new();
    for _ in 0..updates {
        app.update();
        all.extend(events(app).into_iter().map(|(_, event)| event));
    }
    all
}

fn list_files(entries: &str) -> App {
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("a.webm", PatternClip::new(8, 8, 3, 60).to_webm());
    files.insert("b.webm", PatternClip::new(8, 8, 3, 60).to_webm());
    files.insert("a.video.ron", br#"(format_version: 1, file: "a.webm")"#.to_vec());
    files.insert("b.video.ron", br#"(format_version: 1, file: "b.webm")"#.to_vec());
    files.insert("play.videolist.ron", entries.as_bytes().to_vec());
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .add_plugins(MediaPlugin);
    app
}

#[test]
fn playlist_plays_both_entries_then_list_ended_once() {
    let mut app =
        list_files(r#"(format_version: 1, entries: ["a.video.ron", "b.video.ron"], loop_list: false)"#);
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            ..VideoPlayer::new("play.videolist.ron")
        })
        .id();
    let events = collect(&mut app, 40);
    let opened = events.iter().filter(|ev| *ev == &VideoEvent::Opened).count();
    let listed = events.iter().filter(|ev| *ev == &VideoEvent::ListEnded).count();
    assert_eq!(opened, 2, "{events:?}");
    assert_eq!(listed, 1, "{events:?}");
    let playing = app
        .world()
        .get::<VideoPlayerState>(e)
        .expect("state")
        .playing()
        .map(str::to_string);
    assert_eq!(playing.as_deref(), Some("b.video.ron"));
    assert_eq!(
        app.world().get::<VideoPlayerState>(e).expect("state").opened(),
        Some("play.videolist.ron")
    );
    let extra = collect(&mut app, 5);
    assert!(!extra.contains(&VideoEvent::ListEnded));
}

#[test]
fn looping_playlist_does_not_list_end() {
    let mut app =
        list_files(r#"(format_version: 1, entries: ["a.video.ron"], loop_list: true, shuffle: None)"#);
    app.world_mut().spawn(VideoPlayer {
        clock: ClockMode::Lockstep,
        ..VideoPlayer::new("play.videolist.ron")
    });
    let events = collect(&mut app, 40);
    let opened = events.iter().filter(|ev| *ev == &VideoEvent::Opened).count();
    assert!(opened >= 2, "{events:?}");
    assert!(!events.contains(&VideoEvent::ListEnded));
}

#[test]
fn empty_playlist_and_all_failing_entries_fail() {
    let mut empty = list_files(r#"(format_version: 1, entries: [], loop_list: false)"#);
    let e = empty
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            ..VideoPlayer::new("play.videolist.ron")
        })
        .id();
    empty.update();
    let state = empty.world().get::<VideoPlayerState>(e).expect("state");
    assert_eq!(state.core().state(), PlayState::Failed);
    let message = state.core().last_error().expect("error").to_string();
    assert!(message.contains("empty playlist"), "{message}");

    let mut bad = list_files(
        r#"(format_version: 1, entries: ["missing-a.video.ron", "missing-b.video.ron"], loop_list: false)"#,
    );
    let e = bad
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            ..VideoPlayer::new("play.videolist.ron")
        })
        .id();
    let events = collect(&mut bad, 2);
    assert_eq!(
        bad.world()
            .get::<VideoPlayerState>(e)
            .expect("state")
            .core()
            .state(),
        PlayState::Failed
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, VideoEvent::OpenFailed { .. }))
    );
    assert!(events.iter().any(|ev| matches!(ev, VideoEvent::Failed { .. })));
}

#[test]
fn a_failed_later_playlist_entry_keeps_the_open_clip() {
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("a.webm", PatternClip::new(8, 8, 3, 60).to_webm());
    files.insert(
        "a.video.ron",
        br#"(format_version: 1, file: "a.webm", clear_color: (0.0, 0.0, 1.0, 1.0))"#.to_vec(),
    );
    files.insert(
        "bad.video.ron",
        br#"(format_version: 1, file: "missing.webm", clear_color: (1.0, 0.0, 0.0, 1.0))"#.to_vec(),
    );
    files.insert(
        "play.videolist.ron",
        br#"(format_version: 1, entries: ["a.video.ron", "bad.video.ron"], loop_list: false)"#.to_vec(),
    );
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .add_plugins(MediaPlugin);
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            ..VideoPlayer::new("play.videolist.ron")
        })
        .id();
    let events = collect(&mut app, 40);
    assert!(
        events.contains(&VideoEvent::ListEnded),
        "the failed second entry ends the list: {events:?}"
    );
    let state = app.world().get::<VideoPlayerState>(e).expect("state");
    assert_eq!(
        state.playing(),
        Some("a.video.ron"),
        "a failed entry replaced the clip that is still open"
    );
    assert_eq!(
        state.clear_color(),
        [0.0, 0.0, 1.0, 1.0],
        "a failed entry replaced the open clip's clear color: {:?}",
        state.clear_color()
    );
}

#[test]
fn video_ron_loop_and_volume_apply_when_the_component_is_default() {
    use regia_audio::media_feed::MediaAudioRegistry;
    use regia_media::mux::ToneAudio;
    let mut clip = PatternClip::new(8, 8, 4, 60);
    clip.audio = Some(ToneAudio {
        rate: 48_000,
        channels: 1,
        freq_hz: 440.0,
        amplitude: 0.2,
    });
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("tone.webm", clip.to_webm());
    files.insert(
        "tone.video.ron",
        br#"(format_version: 1, file: "tone.webm", volume: 0.25, loop_mode: Loop, preload: WholeFile)"#
            .to_vec(),
    );
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .init_resource::<MediaAudioRegistry>()
        .add_plugins(MediaPlugin);
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            ..VideoPlayer::new("tone.video.ron")
        })
        .id();
    app.update();
    assert_eq!(
        app.world()
            .get::<VideoPlayerState>(e)
            .expect("state")
            .core()
            .loop_mode(),
        LoopMode::Loop
    );
    let feeds = app.world().resource::<MediaAudioRegistry>().feeds();
    assert_eq!(feeds.len(), 1);
    assert!((feeds[0].volume - 0.25).abs() < f32::EPSILON);
    let kept = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            volume: 0.5,
            loop_mode: LoopMode::Off,
            ..VideoPlayer::new("tone.video.ron")
        })
        .id();
    app.update();
    assert_eq!(
        app.world()
            .get::<VideoPlayerState>(kept)
            .expect("state")
            .core()
            .loop_mode(),
        LoopMode::Loop,
        "component Off still takes the asset loop"
    );
    let feeds = app.world().resource::<MediaAudioRegistry>().feeds();
    let row = feeds.iter().find(|f| f.owner == kept.to_bits()).expect("feed");
    assert!((row.volume - 0.5).abs() < f32::EPSILON);
}

#[test]
fn null_audio_is_the_default_until_select_disables_it() {
    use regia_audio::media_feed::MediaAudioRegistry;
    use regia_media::mux::ToneAudio;
    let mut clip = PatternClip::new(8, 8, 4, 30);
    clip.audio = Some(ToneAudio {
        rate: 48_000,
        channels: 1,
        freq_hz: 440.0,
        amplitude: 0.2,
    });
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("tone.webm", clip.to_webm());
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .init_resource::<MediaAudioRegistry>()
        .add_plugins(MediaPlugin);
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            audio_track: None,
            ..VideoPlayer::new("tone.webm")
        })
        .id();
    app.update();
    assert_eq!(app.world().resource::<MediaAudioRegistry>().feeds().len(), 1);
    assert!(
        !app.world()
            .get::<VideoPlayerState>(e)
            .expect("state")
            .audio_disabled(),
        "a fresh None still uses the default track"
    );
    app.world_mut()
        .get_mut::<VideoPlayerState>(e)
        .expect("state")
        .set_audio_disabled(true);
    let player = VideoPlayer {
        clock: ClockMode::Audio,
        audio_track: None,
        ..VideoPlayer::new("tone.webm")
    };
    command_open(app.world_mut(), e, player).expect("reopen");
    assert!(app.world().resource::<MediaAudioRegistry>().feeds().is_empty());
    assert_eq!(
        app.world()
            .get::<VideoPlayerState>(e)
            .expect("state")
            .core()
            .clock(),
        ClockMode::Game
    );
}

/// Spec 50 § 4: loaders reject unknown future versions with an error naming the file.
#[test]
fn a_newer_video_ron_is_refused_naming_the_file() {
    let mut files = MediaFiles::default();
    files.insert(
        "movies/future.video.ron",
        r#"(format_version: 9, file: "movies/pattern.webm")"#.as_bytes().to_vec(),
    );
    let err = files.resolve("movies/future.video.ron").expect_err("refused");
    assert_eq!(err.code(), "container_invalid");
    let text = err.to_string();
    assert!(text.contains("movies/future.video.ron"), "{text}");
    assert!(text.contains("format_version 9"), "{text}");
}

/// Spec 50 § 2.2 edge case "a rate of 0.0 is pause": a component opened with `rate: 0.0` and
/// `autoplay` waits in Ready instead of playing at the previous rate.
#[test]
fn a_component_rate_of_zero_opens_paused() {
    let mut app = app_with_clip(5);
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            rate: 0.0,
            ..VideoPlayer::new("movies/pattern.webm")
        })
        .id();
    for _ in 0..3 {
        app.update();
    }
    let state = app.world().get::<VideoPlayerState>(e).expect("state");
    assert_eq!(state.core().state(), PlayState::Ready, "rate 0 holds the player");
    assert_eq!(state.core().frame_index(), Some(0));
}

/// Spec 50 § 3.4: events reach the message stream sorted by entity bits, whatever the query order
/// (here the first player moved to a newer archetype, so the query visits it last).
#[test]
fn messages_are_sorted_by_entity_bits() {
    #[derive(Component)]
    struct Marker;
    let mut app = app_with_clip(5);
    let lockstep = || VideoPlayer {
        clock: ClockMode::Lockstep,
        ..VideoPlayer::new("movies/pattern.webm")
    };
    let e0 = app.world_mut().spawn(lockstep()).id();
    let e1 = app.world_mut().spawn((lockstep(), Name::new("second"))).id();
    app.world_mut().entity_mut(e0).insert(Marker);
    app.update();
    let mut opened: Vec<Entity> = events(&app)
        .into_iter()
        .filter(|(_, ev)| *ev == VideoEvent::Opened)
        .map(|(e, _)| e)
        .collect();
    assert_eq!(opened.len(), 2);
    let in_stream = opened.clone();
    opened.sort_by_key(|e| e.to_bits());
    assert_eq!(in_stream, opened, "sorted by entity bits");
    assert!(in_stream.contains(&e0) && in_stream.contains(&e1));
}

/// Spec 50 § 5.1: property edits (`rate`, `loop`, `clock`) on the component reach a running session;
/// an unsupported rate is ignored.
#[test]
fn component_property_edits_reach_the_running_session() {
    let mut app = app_with_clip(30);
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            ..VideoPlayer::new("movies/pattern.webm")
        })
        .id();
    app.update();
    {
        let mut p = app.world_mut().get_mut::<VideoPlayer>(e).expect("player");
        p.rate = 2.0;
        p.loop_mode = LoopMode::Loop;
        p.clock = ClockMode::Game;
    }
    app.update();
    {
        let state = app.world().get::<VideoPlayerState>(e).expect("state");
        let core = state.core();
        assert_eq!(core.rate(), 2.0);
        assert_eq!(core.loop_mode(), LoopMode::Loop);
        assert_eq!(core.clock(), ClockMode::Game);
    }
    app.world_mut().get_mut::<VideoPlayer>(e).expect("player").rate = 9.0;
    app.update();
    let state = app.world().get::<VideoPlayerState>(e).expect("state");
    assert_eq!(state.core().rate(), 2.0, "an unsupported rate is ignored");
}

/// The picture size and pattern index on screen, or `None` before the first picture.
fn shown_entry(app: &mut App, e: Entity) -> Option<(u32, u32)> {
    let mut state = app.world_mut().get_mut::<VideoPlayerState>(e).expect("state");
    let core = state.core_mut();
    core.presented().map(|p| (p.width, pattern_index(&p.planes[0])))
}

/// Two entries of different widths (8 and 16) so the picture tells which entry plays.
fn sized_list_files() -> App {
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("a.webm", PatternClip::new(8, 8, 10, 60).to_webm());
    files.insert("b.webm", PatternClip::new(16, 8, 30, 60).to_webm());
    files.insert("a.video.ron", br#"(format_version: 1, file: "a.webm")"#.to_vec());
    files.insert("b.video.ron", br#"(format_version: 1, file: "b.webm")"#.to_vec());
    files.insert(
        "play.videolist.ron",
        br#"(format_version: 1, entries: ["a.video.ron", "b.video.ron"], loop_list: false)"#.to_vec(),
    );
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .add_plugins(MediaPlugin);
    app
}

/// Updates until `entry` plays (at most 60 updates).
fn until_playing(app: &mut App, e: Entity, entry: &str) {
    for _ in 0..60 {
        if app.world().get::<VideoPlayerState>(e).and_then(|s| s.playing()) == Some(entry) {
            return;
        }
        app.update();
    }
    panic!("{entry} never played");
}

/// `phase_offset` is the initial seek (specs/50 § 2.6): the first entry opens there, a later
/// entry opens at its start.
#[test]
fn only_the_first_playlist_entry_seeks_to_the_phase_offset() {
    let mut app = sized_list_files();
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            phase_offset: 4.5 / 60.0,
            ..VideoPlayer::new("play.videolist.ron")
        })
        .id();
    app.update();
    let first = shown_entry(&mut app, e).expect("entry a shows a picture");
    assert_eq!(first.0, 8, "entry a plays first");
    assert!(first.1 >= 4, "entry a starts at the phase offset: {first:?}");
    until_playing(&mut app, e, "b.video.ron");
    let b = shown_entry(&mut app, e).expect("entry b shows a picture");
    assert_eq!(b.0, 16, "entry b plays");
    assert!(
        b.1 < 4,
        "entry b starts at 0, not at the list's phase offset: {b:?}"
    );
}

/// `video.select_track` on a playlist reopens the entry that plays now, at the given position.
/// It does not restart the list at entry 0.
#[test]
fn reopening_tracks_on_a_playlist_keeps_the_current_entry() {
    use regia_media::plugin::command_reopen_tracks;
    let mut app = sized_list_files();
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            ..VideoPlayer::new("play.videolist.ron")
        })
        .id();
    until_playing(&mut app, e, "b.video.ron");
    for _ in 0..3 {
        app.update();
    }
    let mut player = app.world().get::<VideoPlayer>(e).cloned().expect("player");
    player.phase_offset = 10.5 / 60.0;
    let state = command_reopen_tracks(app.world_mut(), e, player).expect("reopen");
    assert_ne!(state, PlayState::Failed);
    let st = app.world().get::<VideoPlayerState>(e).expect("state");
    assert_eq!(st.playing(), Some("b.video.ron"), "the list restarted");
    assert_eq!(st.opened(), Some("play.videolist.ron"));
    let shown = shown_entry(&mut app, e).expect("picture after the reopen");
    assert_eq!(shown.0, 16, "entry b is on screen after the reopen: {shown:?}");
    assert!(shown.1 >= 10, "the reopen seeks to the position: {shown:?}");
    // The list still ends after b, once.
    let events = collect(&mut app, 60);
    let listed = events.iter().filter(|ev| *ev == &VideoEvent::ListEnded).count();
    assert_eq!(listed, 1, "{events:?}");
}
