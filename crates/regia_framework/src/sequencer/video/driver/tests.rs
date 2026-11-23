// SPDX-License-Identifier: MPL-2.0
//! Video sections end to end in a headless app, on the media crate's own pattern clips (synthetic,
//! no third-party content; each picture carries its frame index in its first two luma bytes). No
//! test here asserts on pixels beyond that index, and none needs a GPU.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use regia_media::mux::{PatternClip, ToneAudio, pattern_index};
use regia_media::session::{FeedPoll, FeedStats, FrameFeed};
use regia_scene::Transform;

use super::super::super::asset::{Channel, Sequence, SubSection, Track, VideoSection};
use super::super::super::player::{Binding, EndPolicy, SequenceBindings, SequencerPlugin};
use super::*;

/// 60 fps, so 16.67 ms frames whose millisecond stamps round both ways.
const CLIP: &str = "movies/clip.webm";

fn clip(frames: u32, fps: u32) -> PatternClip {
    PatternClip::new(16, 8, frames, fps)
}

/// A manual-clock app with the video sections' plugins and one clip per entry.
fn app_with(files: &[(&str, Vec<u8>)]) -> App {
    let mut app = App::new();
    let mut media = MediaFiles::default();
    for (path, bytes) in files {
        media.insert(*path, bytes.clone());
    }
    app.add_plugins(MinimalPlugins)
        .insert_resource(media)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_nanos(
            33_333_333,
        )))
        .add_plugins((regia_media::MediaPlugin, regia_media::VideoFramePlugin))
        .add_plugins(SequencerPlugin);
    app
}

fn video_track(name: &str, sections: Vec<VideoSection>) -> Track {
    Track {
        name: name.into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Video(sections),
        enabled: true,
        locked: false,
    }
}

/// `Hold` so the end of the sequence still shows, on the external clock like render-out.
fn external(sequence: Sequence) -> SequencePlayer {
    let mut player = SequencePlayer::new(sequence);
    player.clock = ClockSource::External;
    player.end = EndPolicy::Hold;
    player.takeover.enabled = false;
    player.play();
    player
}

fn sequence(tracks: Vec<Track>) -> Sequence {
    Sequence {
        tracks,
        ..Sequence::default()
    }
}

fn set_time(app: &mut App, player: Entity, t: f64) {
    app.world_mut()
        .get_mut::<SequencePlayer>(player)
        .expect("player")
        .set_external_time(t);
    app.update();
}

fn status(app: &App, player: Entity) -> Vec<VideoSectionStatus> {
    app.world().resource::<SequenceVideos>().sections(player)
}

/// The index carried by the picture a section's session presents, read from the picture itself.
fn shown(app: &mut App, player: Entity, section: usize) -> Option<u32> {
    let media = status(app, player).get(section)?.media?;
    let mut state = app.world_mut().get_mut::<VideoPlayerState>(media)?;
    let core = state.core_mut();
    core.presented().map(|p| pattern_index(&p.planes[0]))
}

fn issues(app: &App, player: Entity) -> Vec<&'static str> {
    app.world()
        .get::<SequencePlayer>(player)
        .expect("player")
        .issues()
        .map(SequenceIssue::kind)
        .collect()
}

/// The index a clip of `fps` shows at source time `t`: the nominal frame, floor.
fn ideal(t: f64, fps: f64, frames: u32) -> u32 {
    ((t * fps + 1.0e-6).floor() as u32).min(frames - 1)
}

/// The brief's acceptance: a 2 s sequence on the job's external clock at 30 fps, a section of a
/// 60 fps clip from 0.5 s to 1.5 s, 0.25 s into the source. Each output frame shows the expected
/// source frame, from the pictures themselves, and nothing was ever dropped.
#[test]
fn two_seconds_of_output_show_the_expected_source_frame_on_every_frame() {
    for (fps, frames) in [(60u32, 180u32), (24, 72), (30, 90), (50, 150)] {
        let bytes = clip(frames, fps).to_webm();
        let mut app = app_with(&[(CLIP, bytes)]);
        let mut section = VideoSection::new(CLIP, 0.5, 1.5);
        section.offset = 0.25;
        let hold = Track {
            name: "tail".into(),
            target: "ui".into(),
            property: "opacity".into(),
            keys: Channel::Float(vec![super::super::super::asset::ScalarKey::new(
                2.0,
                1.0,
                super::super::super::asset::Ease::Linear,
            )]),
            enabled: true,
            locked: false,
        };
        let p = app
            .world_mut()
            .spawn(external(sequence(vec![video_track("wall", vec![section]), hold])))
            .id();
        for output in 0..60u32 {
            let t = f64::from(output) / 30.0;
            set_time(&mut app, p, t);
            let state = status(&app, p);
            if (0.5..1.5).contains(&t) {
                let want = ideal(t - 0.5 + 0.25, f64::from(fps), frames);
                assert_eq!(
                    shown(&mut app, p, 0),
                    Some(want),
                    "{fps} fps, output frame {output}"
                );
                assert!(state[0].exact && state[0].certain, "{state:?}");
                assert_eq!(state[0].drift, 0.0, "{state:?}");
                assert_eq!(state[0].frames_dropped, 0);
                assert_eq!(
                    state[0].frame_shown,
                    Some(u64::from(want)),
                    "nominal index, {fps} fps"
                );
            } else {
                assert!(state.is_empty(), "no section at {t}");
            }
        }
        assert_eq!(app.world().resource::<SequenceVideos>().opened(), 1);
        assert_eq!(app.world().resource::<SequenceVideos>().closed(), 1);
    }
}

fn indices(app: &mut App, p: Entity, times: &[f64]) -> Vec<Option<u32>> {
    times
        .iter()
        .map(|&t| {
            set_time(app, p, t);
            shown(app, p, 0)
        })
        .collect()
}

/// The brief's acceptance: seeking backward then forward shows the same frame as linear play, with
/// and without cues, and whichever way a time is reached. Scattered visits too.
#[test]
fn a_seek_backward_then_forward_shows_what_linear_play_shows() {
    for cues in [true, false] {
        let mut source = clip(240, 60);
        source.cues = cues;
        let mut app = app_with(&[(CLIP, source.to_webm())]);
        let p = app
            .world_mut()
            .spawn(external(sequence(vec![video_track(
                "wall",
                vec![VideoSection::new(CLIP, 0.0, 4.0)],
            )])))
            .id();
        let times: Vec<f64> = (0..120).map(|i| f64::from(i) / 30.0).collect();
        let linear = indices(&mut app, p, &times);
        assert_eq!(linear[3], Some(6));
        assert_eq!(linear[100], Some(200));
        let backward: Vec<f64> = times.iter().rev().copied().collect();
        let mut seen = indices(&mut app, p, &backward);
        seen.reverse();
        assert_eq!(seen, linear, "backward, cues {cues}");
        assert_eq!(indices(&mut app, p, &times), linear, "forward again, cues {cues}");
        for i in 0..240u64 {
            let k = ((i * 7919) % 120) as usize;
            set_time(&mut app, p, times[k]);
            assert_eq!(
                shown(&mut app, p, 0),
                linear[k],
                "scattered visit {i}: {}",
                times[k]
            );
        }
        let state = status(&app, p);
        assert!(state[0].seeks > 100, "it did seek: {state:?}");
        assert_eq!(state[0].frames_dropped, 0);
    }
}

/// Render-out's determinism: two runs show the same pictures on the same frames, even when the
/// second run's output frames land between the first run's seeks.
#[test]
fn two_runs_show_the_same_pictures() {
    let run = || {
        let mut app = app_with(&[(CLIP, clip(120, 60).to_webm())]);
        let mut section = VideoSection::new(CLIP, 0.25, 1.75);
        section.looping = true;
        let p = app
            .world_mut()
            .spawn(external(sequence(vec![video_track("wall", vec![section])])))
            .id();
        let mut hashes = Vec::new();
        for output in 0..60u32 {
            set_time(&mut app, p, f64::from(output) / 30.0);
            let Some(media) = status(&app, p).first().and_then(|s| s.media) else {
                hashes.push(0);
                continue;
            };
            let mut state = app
                .world_mut()
                .get_mut::<VideoPlayerState>(media)
                .expect("session");
            let core = state.core_mut();
            let picture = core.presented().expect("a picture");
            hashes.push(regia_media::reference::fnv1a64(&picture.planes[0]));
        }
        hashes
    };
    let first = run();
    assert!(first.iter().any(|h| *h != 0));
    assert_eq!(first, run());
}

/// The brief's acceptance: a missing video warns `seq_video_missing` and the rest of the sequence
/// plays: its float track applies, its marker fires, and its other video shows.
#[test]
fn a_missing_video_warns_and_the_rest_of_the_sequence_plays() {
    use super::super::super::asset::{Ease, Marker, Vec3Key};
    use super::super::super::player::MarkerReached;
    let mut app = app_with(&[(CLIP, clip(120, 60).to_webm())]);
    let hero = app.world_mut().spawn(Transform::default()).id();
    let moves = Track {
        name: "move".into(),
        target: "hero".into(),
        property: "transform.position".into(),
        keys: Channel::Vec3(vec![
            Vec3Key::new(0.0, [0.0, 0.0, 0.0], Ease::Linear),
            Vec3Key::new(1.0, [10.0, 0.0, 0.0], Ease::Linear),
        ]),
        enabled: true,
        locked: false,
    };
    let mut seq = sequence(vec![
        video_track("gone", vec![VideoSection::new("movies/gone.webm", 0.0, 1.0)]),
        video_track("here", vec![VideoSection::new(CLIP, 0.0, 1.0)]),
        moves,
    ]);
    seq.markers.push(Marker {
        name: "cue".into(),
        time: 0.25,
        payload: String::new(),
    });
    let player = external(seq);
    let bindings = SequenceBindings::default().with("hero", Binding::Entity(hero));
    let p = app.world_mut().spawn((player, bindings)).id();
    for output in 0..16u32 {
        set_time(&mut app, p, f64::from(output) / 30.0);
    }
    assert_eq!(issues(&app, p), ["seq_video_missing"], "once, and nothing else");
    let state = status(&app, p);
    assert_eq!(state[0].error.as_deref(), Some("source_not_found"));
    assert_eq!(state[0].media, None);
    assert_eq!(state[0].state, "closed");
    assert_eq!(shown(&mut app, p, 1), Some(30), "the other video plays");
    let x = app.world().get::<Transform>(hero).expect("hero").translation.x;
    assert!((x - 5.0).abs() < 1.0e-3, "the keyed track plays: {x}");
    let fired = app.world().resource::<Messages<MarkerReached>>();
    assert_eq!(
        app.world()
            .get::<SequencePlayer>(p)
            .expect("player")
            .markers_fired(),
        1
    );
    assert!(fired.iter_current_update_messages().count() <= 1);
    assert_eq!(
        app.world().resource::<SequenceVideos>().opened(),
        2,
        "the missing source is not tried again every frame"
    );
    // Other failures say what happened: a patented codec's container.
    let mut app = app_with(&[("movies/clip.mp4", vec![0u8; 16])]);
    let p = app
        .world_mut()
        .spawn(external(sequence(vec![video_track(
            "bad",
            vec![VideoSection::new("movies/clip.mp4", 0.0, 1.0)],
        )])))
        .id();
    set_time(&mut app, p, 0.1);
    assert_eq!(issues(&app, p), ["seq_video_failed"]);
    // An edit that fixes the path brings the picture back and forgets the issue.
    let mut fixed = sequence(vec![video_track("bad", vec![VideoSection::new(CLIP, 0.0, 1.0)])]);
    fixed.name = "fixed".into();
    app.world_mut()
        .resource_mut::<MediaFiles>()
        .insert(CLIP, clip(120, 60).to_webm());
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .replace_sequence(fixed);
    set_time(&mut app, p, 0.25);
    assert_eq!(shown(&mut app, p, 0), Some(15));
    assert!(issues(&app, p).is_empty());
}

/// Looping wraps over the source's length on the frame the source ends; without it the last frame
/// holds until the section ends.
#[test]
fn a_looping_section_wraps_and_a_plain_one_holds_its_last_frame() {
    let bytes = clip(30, 30).to_webm();
    let mut looping = VideoSection::new(CLIP, 0.0, 3.0);
    looping.looping = true;
    let plain = VideoSection::new(CLIP, 0.0, 3.0);
    for (section, want) in [
        (looping, (0..90).map(|k| k % 30).collect::<Vec<u32>>()),
        (plain, (0..90).map(|k: u32| k.min(29)).collect()),
    ] {
        let mut app = app_with(&[(CLIP, bytes.clone())]);
        let p = app
            .world_mut()
            .spawn(external(sequence(vec![video_track("wall", vec![section])])))
            .id();
        let seen: Vec<u32> = (0..90u32)
            .map(|k| {
                set_time(&mut app, p, f64::from(k) / 30.0);
                shown(&mut app, p, 0).expect("a picture")
            })
            .collect();
        assert_eq!(seen, want);
    }
}

/// The offset carries past a section seam, and a sub-sequence's speed reaches the picture.
#[test]
fn sections_under_a_sub_sequence_follow_its_clock() {
    let mut app = app_with(&[(CLIP, clip(240, 60).to_webm())]);
    let inner = sequence(vec![video_track("wall", vec![VideoSection::new(CLIP, 0.0, 2.0)])]);
    let mut library = super::super::super::sample::SequenceLibrary::default();
    library.insert("inner", inner);
    app.insert_resource(library.clone());
    let mut section = SubSection::new("inner", 0.5, 1.5);
    section.speed = 2.0;
    let mut player = external(sequence(vec![Track {
        name: "fx".into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Sub(vec![section]),
        enabled: true,
        locked: false,
    }]));
    player.set_library(&library);
    assert!(player.has_video_sections());
    let p = app.world_mut().spawn(player).id();
    set_time(&mut app, p, 1.0);
    assert_eq!(shown(&mut app, p, 0), Some(60), "0.5 s in at speed 2");
    set_time(&mut app, p, 1.25);
    assert_eq!(shown(&mut app, p, 0), Some(90));
}

/// Sessions close with the player: a stop, a despawn, and the end of a `Once` player.
#[test]
fn sessions_close_with_their_player() {
    let mut app = app_with(&[(CLIP, clip(120, 60).to_webm())]);
    let spawned = |app: &App| {
        let mut count = 0;
        for archetype in app.world().archetypes().iter() {
            if app
                .world()
                .components()
                .component_id::<VideoPlayer>()
                .is_some_and(|id| archetype.contains(id))
            {
                count += archetype.len();
            }
        }
        count
    };
    let seq = || sequence(vec![video_track("wall", vec![VideoSection::new(CLIP, 0.0, 1.0)])]);
    let p = app.world_mut().spawn(external(seq())).id();
    set_time(&mut app, p, 0.5);
    assert_eq!(spawned(&app), 1);
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .stop();
    app.update();
    assert_eq!(spawned(&app), 0, "stop closes");
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .play();
    set_time(&mut app, p, 0.5);
    assert_eq!(spawned(&app), 1);
    app.world_mut().despawn(p);
    app.update();
    assert_eq!(spawned(&app), 0, "a despawned player closes");
    assert!(app.world().resource::<SequenceVideos>().is_empty());
    let mut once = external(seq());
    once.end = EndPolicy::Once;
    let p = app.world_mut().spawn(once).id();
    set_time(&mut app, p, 0.5);
    assert_eq!(spawned(&app), 1);
    set_time(&mut app, p, 2.0);
    assert_eq!(spawned(&app), 0, "a `Once` player's end closes");
}

/// A pause holds the sound and the picture; a scrub moves the picture, in frame-exact and real-time
/// sessions alike; play resumes the sound.
#[test]
fn a_paused_player_holds_its_sound_and_follows_a_scrub() {
    let mut app = app_with(&[(CLIP, clip(240, 60).to_webm())]);
    let p = app
        .world_mut()
        .spawn(external(sequence(vec![video_track(
            "wall",
            vec![VideoSection::new(CLIP, 0.0, 4.0)],
        )])))
        .id();
    set_time(&mut app, p, 1.0);
    assert_eq!(status(&app, p)[0].state, "playing");
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .pause();
    app.update();
    assert_eq!(status(&app, p)[0].state, "paused");
    let mut player = app.world_mut().get_mut::<SequencePlayer>(p).expect("player");
    player.seek(0.5);
    app.update();
    assert_eq!(shown(&mut app, p, 0), Some(30));
    assert_eq!(status(&app, p)[0].state, "paused");
    let mut player = app.world_mut().get_mut::<SequencePlayer>(p).expect("player");
    player.seek(2.0);
    app.update();
    assert_eq!(shown(&mut app, p, 0), Some(120));
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .play();
    app.update();
    assert_eq!(status(&app, p)[0].state, "playing");
}

/// Render-out holds the sound during its pre-roll.
#[test]
fn hold_audio_keeps_the_session_silent_but_shows_the_picture() {
    let mut app = app_with(&[(CLIP, clip(120, 60).to_webm())]);
    let mut player = external(sequence(vec![video_track(
        "wall",
        vec![VideoSection::new(CLIP, 0.0, 2.0)],
    )]));
    player.hold_audio = true;
    let p = app.world_mut().spawn(player).id();
    set_time(&mut app, p, 0.5);
    assert_eq!(shown(&mut app, p, 0), Some(30));
    assert_eq!(status(&app, p)[0].state, "ready");
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .hold_audio = false;
    set_time(&mut app, p, 0.75);
    assert_eq!(status(&app, p)[0].state, "playing");
}

/// The decode budget bounds a frame-exact frame; past it the issue says so and later frames land.
#[test]
fn an_exact_seek_past_its_budget_reports_it_and_lands_later() {
    let mut source = clip(600, 60);
    source.cues = false;
    let mut app = app_with(&[(CLIP, source.to_webm())]);
    app.world_mut().resource_mut::<SequenceVideos>().exact_budget = 64;
    let p = app
        .world_mut()
        .spawn(external(sequence(vec![video_track(
            "wall",
            vec![VideoSection::new(CLIP, 0.0, 10.0)],
        )])))
        .id();
    set_time(&mut app, p, 5.0);
    assert!(!status(&app, p)[0].certain);
    assert_eq!(issues(&app, p), ["seq_video_failed"]);
    assert_ne!(shown(&mut app, p, 0), Some(300));
    let mut frames = 0;
    while !status(&app, p)[0].certain {
        app.update();
        frames += 1;
        assert!(frames < 20, "never landed");
    }
    assert_eq!(shown(&mut app, p, 0), Some(300));
}

/// Real time: a session on the decode thread never blocks the caller, lands on the same pictures and
/// reports the drift it had to wait through. The loop counts frames, it does not time them.
#[test]
fn a_real_time_session_lands_without_blocking() {
    let mut app = app_with(&[(CLIP, clip(240, 60).to_webm())]);
    app.insert_resource(TimeUpdateStrategy::Automatic);
    let p = app
        .world_mut()
        .spawn(external(sequence(vec![video_track(
            "wall",
            vec![VideoSection::new(CLIP, 0.0, 4.0)],
        )])))
        .id();
    let settle = |app: &mut App, want: u32| {
        let mut frames = 0u32;
        while !(status(app, p)[0].certain && shown(app, p, 0) == Some(want)) {
            app.update();
            std::thread::yield_now();
            frames += 1;
            assert!(frames < 2_000_000, "never landed on {want}: {:?}", status(app, p));
        }
    };
    set_time(&mut app, p, 2.0);
    assert!(!status(&app, p)[0].exact);
    settle(&mut app, 120);
    set_time(&mut app, p, 0.5);
    settle(&mut app, 30);
    set_time(&mut app, p, 3.5);
    settle(&mut app, 210);
    assert!(status(&app, p)[0].seeks >= 3, "{:?}", status(&app, p));
    // A manual clock makes the running session frame-exact: it reopens on this thread.
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(33)));
    set_time(&mut app, p, 1.0);
    let state = status(&app, p);
    assert!(state[0].exact && state[0].certain, "{state:?}");
    assert_eq!(shown(&mut app, p, 0), Some(60));
}

/// A feed that delivers one picture and then stalls, counting seeks: a decoder too slow for its clip.
struct StalledFeed {
    first: Option<regia_media::codec::DecodedPicture>,
    seeks: Arc<AtomicU64>,
}

impl FrameFeed for StalledFeed {
    fn poll(&mut self, _block: bool) -> FeedPoll {
        self.first.take().map_or(FeedPoll::Pending, FeedPoll::Frame)
    }

    fn seek(&mut self, _to_ns: u64) {
        self.seeks.fetch_add(1, Ordering::Relaxed);
    }

    fn stats(&self) -> FeedStats {
        FeedStats::default()
    }
}

/// Drift correction end to end: the sequence clock runs on while the decoder stalls, so the picture
/// falls behind. Past the limit the session repositions once, then waits out its cooldown: no storm
/// of seeks. The drift is reported, the clock is never held, and the player is not buffering.
#[test]
fn a_stalled_decoder_is_repositioned_at_most_once_per_cooldown() {
    let bytes = clip(600, 60).to_webm();
    let mut app = app_with(&[(CLIP, bytes.clone())]);
    app.insert_resource(TimeUpdateStrategy::Automatic);
    let p = app
        .world_mut()
        .spawn(external(sequence(vec![video_track(
            "wall",
            vec![VideoSection::new(CLIP, 0.0, 10.0)],
        )])))
        .id();
    set_time(&mut app, p, 0.0);
    let media = status(&app, p)[0].media.expect("a session");
    // The session's real feed gives way to the stalled one.
    let options = SessionOptions::default();
    let mut session = Session::open(std::io::Cursor::new(bytes), &options, None).expect("opens");
    let info = session.info().clone();
    let FeedPoll::Frame(first) = session.poll(true) else {
        panic!("a first picture");
    };
    let seeks = Arc::new(AtomicU64::new(0));
    let feed = StalledFeed {
        first: Some(first),
        seeks: seeks.clone(),
    };
    app.world_mut()
        .get_mut::<VideoPlayerState>(media)
        .expect("session")
        .core_mut()
        .open(
            Box::new(feed),
            info,
            ClockMode::External,
            LoopMode::Off,
            false,
            0.0,
        );
    let resyncs_at = |app: &mut App, t: f64| {
        set_time(app, p, t);
        let state = status(app, p);
        assert_eq!(
            state[0].resyncs,
            seeks.load(Ordering::Relaxed),
            "at {t}: {state:?}"
        );
        assert_ne!(state[0].state, "buffering", "at {t}");
        assert_eq!(state[0].seeks, 0, "the clock did not jump, at {t}");
        state[0].resyncs
    };
    assert_eq!(
        resyncs_at(&mut app, 0.1),
        0,
        "a picture late by less than the limit"
    );
    assert_eq!(resyncs_at(&mut app, 0.2), 0);
    assert_eq!(resyncs_at(&mut app, 0.4), 1, "past the limit: once");
    let drift = status(&app, p)[0].drift;
    assert!(
        (drift - (0.4 - 1.0 / 60.0)).abs() < 1.0e-3,
        "the lag past one frame period: {drift}"
    );
    assert!(!status(&app, p)[0].certain);
    for t in [0.6, 0.8, 1.0, 1.2, 1.35] {
        assert_eq!(resyncs_at(&mut app, t), 1, "still cooling down at {t}");
    }
    assert_eq!(resyncs_at(&mut app, 1.5), 2, "a second past the last");
    assert_eq!(resyncs_at(&mut app, 2.4), 2);
    assert_eq!(resyncs_at(&mut app, 2.6), 3);
}

#[test]
fn drift_correction_waits_for_its_cooldown_and_its_limits() {
    assert!(!needs_resync(0.0, None, 100.0));
    assert!(!needs_resync(0.2, Some(0.05), 100.0));
    assert!(needs_resync(0.3, None, 100.0), "the picture is late");
    assert!(needs_resync(0.0, Some(0.2), 100.0), "the sound is late");
    assert!(needs_resync(0.0, Some(-0.2), 100.0), "the sound is early");
    assert!(!needs_resync(0.3, Some(0.2), 0.5), "not again so soon");
}

#[test]
fn media_time_rounds_up_half_a_millisecond_and_wraps() {
    assert_eq!(media_time_ns(0.0, false, None), 500_000);
    assert_eq!(media_time_ns(-1.0, false, Some(1.0)), 0, "never negative");
    assert_eq!(media_time_ns(f64::NAN, false, None), 0);
    let near = |t: u64, want: u64| t.abs_diff(want) <= 2;
    assert!(near(media_time_ns(1.5, true, Some(1.0)), 500_500_000));
    assert!(
        near(media_time_ns(1.0, false, Some(1.0)), 1_000_500_000),
        "no wrap, past the end"
    );
    assert!(
        near(media_time_ns(1.5, true, None), 1_500_500_000),
        "the length is not known yet"
    );
    // Just short of the loop end in floating point: the wrap still lands on the first frame.
    assert!(near(media_time_ns(0.999_999_9, true, Some(1.0)), 499_900));
    assert_eq!(volume_gain(0.0), 1.0);
    assert_eq!(volume_gain(-80.0), 0.0);
    assert!((volume_gain(-6.0) - 0.501).abs() < 1.0e-3);
}

/// A bound entity's own player is taken over and put back; one without a player loses it again.
#[test]
fn a_player_target_is_taken_over_and_restored() {
    let mut app = app_with(&[
        (CLIP, clip(120, 60).to_webm()),
        ("movies/attract.webm", clip(10, 30).to_webm()),
    ]);
    let own = VideoPlayer {
        volume: 0.25,
        ..VideoPlayer::new("movies/attract.webm")
    };
    let kiosk = app.world_mut().spawn(own.clone()).id();
    let bare = app.world_mut().spawn_empty().id();
    let on = |binding: &str, start: f32, end: f32| VideoSection {
        target: VideoTarget::Player(binding.into()),
        ..VideoSection::new(CLIP, start, end)
    };
    let player = external(sequence(vec![video_track(
        "wall",
        vec![on("kiosk", 0.0, 1.0), on("bare", 0.0, 1.0)],
    )]));
    let bindings = SequenceBindings::default()
        .with("kiosk", Binding::Entity(kiosk))
        .with("bare", Binding::Entity(bare));
    let p = app.world_mut().spawn((player, bindings)).id();
    app.update();
    set_time(&mut app, p, 0.5);
    let state = status(&app, p);
    assert_eq!((state[0].media, state[1].media), (Some(kiosk), Some(bare)));
    assert_eq!(shown(&mut app, p, 0), Some(30));
    assert_eq!(app.world().get::<VideoPlayer>(kiosk).expect("kiosk").source, CLIP);
    // Past the sections (the sequence holds, but the clock is out of them).
    let mut seq = sequence(vec![video_track(
        "wall",
        vec![on("kiosk", 0.0, 1.0), on("bare", 0.0, 1.0)],
    )]);
    seq.markers.push(super::super::super::asset::Marker {
        name: "end".into(),
        time: 2.0,
        payload: String::new(),
    });
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .replace_sequence(seq);
    set_time(&mut app, p, 1.5);
    assert!(status(&app, p).is_empty());
    assert_eq!(
        app.world().get::<VideoPlayer>(kiosk),
        Some(&own),
        "what it played"
    );
    assert!(app.world().get::<VideoPlayer>(bare).is_none(), "it had none");
    app.update();
    let kiosk_state = app.world().get::<VideoPlayerState>(kiosk).expect("state");
    assert_eq!(
        kiosk_state.opened(),
        Some("movies/attract.webm"),
        "it plays its own clip again"
    );
}

/// A section on an entity with no mesh says so, and plays on.
#[test]
fn a_screen_target_without_a_mesh_is_reported() {
    let mut app = app_with(&[(CLIP, clip(120, 60).to_webm())]);
    let bare = app.world_mut().spawn_empty().id();
    let section = VideoSection {
        target: VideoTarget::Screen {
            binding: "tv".into(),
            slot: VideoScreenSlot::Emissive,
        },
        ..VideoSection::new(CLIP, 0.0, 1.0)
    };
    let player = external(sequence(vec![video_track("wall", vec![section])]));
    let bindings = SequenceBindings::default().with("tv", Binding::Entity(bare));
    let p = app.world_mut().spawn((player, bindings)).id();
    app.update();
    set_time(&mut app, p, 0.5);
    assert_eq!(issues(&app, p), ["seq_video_failed"]);
    let message = app
        .world()
        .get::<SequencePlayer>(p)
        .expect("player")
        .issues()
        .map(ToString::to_string)
        .collect::<String>();
    assert!(message.contains("material_missing_video_slot"), "{message}");
    assert_eq!(shown(&mut app, p, 0), Some(30));
}

/// A bound player that was already playing gives its session and its sound way.
#[test]
fn a_bound_players_own_sound_gives_way() {
    let mut toned = clip(120, 60);
    toned.audio = Some(ToneAudio {
        rate: 48_000,
        channels: 1,
        freq_hz: 440.0,
        amplitude: 0.5,
    });
    let bytes = toned.to_webm();
    let mut app = app_with(&[(CLIP, bytes.clone()), ("movies/attract.webm", bytes)]);
    app.init_resource::<MediaAudioRegistry>();
    let kiosk = app
        .world_mut()
        .spawn(VideoPlayer::new("movies/attract.webm"))
        .id();
    app.update();
    let feeds = |app: &App| app.world().resource::<MediaAudioRegistry>().feeds().len();
    assert_eq!(feeds(&app), 1, "its own sound");
    let section = VideoSection {
        target: VideoTarget::Player("kiosk".into()),
        ..VideoSection::new(CLIP, 0.0, 1.0)
    };
    let player = external(sequence(vec![video_track("wall", vec![section])]));
    let bindings = SequenceBindings::default().with("kiosk", Binding::Entity(kiosk));
    let p = app.world_mut().spawn((player, bindings)).id();
    app.update();
    set_time(&mut app, p, 0.5);
    assert_eq!(feeds(&app), 1, "the section's, in place of its own");
    assert_eq!(shown(&mut app, p, 0), Some(30));
}

/// Two sections on one entity: the later one has it.
#[test]
fn the_later_section_has_a_shared_target() {
    let mut app = app_with(&[
        (CLIP, clip(120, 60).to_webm()),
        ("movies/b.webm", clip(120, 60).to_webm()),
    ]);
    let kiosk = app.world_mut().spawn_empty().id();
    let on = |video: &str| VideoSection {
        target: VideoTarget::Player("kiosk".into()),
        ..VideoSection::new(video, 0.0, 1.0)
    };
    let player = external(sequence(vec![
        video_track("first", vec![on(CLIP)]),
        video_track("second", vec![on("movies/b.webm")]),
    ]));
    let bindings = SequenceBindings::default().with("kiosk", Binding::Entity(kiosk));
    let p = app.world_mut().spawn((player, bindings)).id();
    app.update();
    set_time(&mut app, p, 0.5);
    let state = status(&app, p);
    assert_eq!(state.len(), 1);
    assert_eq!(state[0].track, "second");
}

/// An unbound target is reported as one and its section waits for it.
#[test]
fn an_unbound_target_waits() {
    let mut app = app_with(&[(CLIP, clip(120, 60).to_webm())]);
    let section = VideoSection {
        target: VideoTarget::UiImage("slate".into()),
        ..VideoSection::new(CLIP, 0.0, 1.0)
    };
    let p = app
        .world_mut()
        .spawn(external(sequence(vec![video_track("wall", vec![section])])))
        .id();
    set_time(&mut app, p, 0.5);
    assert!(status(&app, p).is_empty());
    assert_eq!(issues(&app, p), ["unbound_target"]);
}

/// Without the plugin that shows them the sections say what is missing, and without the media
/// plugin too. The clock goes on.
#[test]
fn what_an_app_lacks_is_reported() {
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert(CLIP, clip(120, 60).to_webm());
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(33)))
        .add_plugins((regia_media::MediaPlugin, SequencerPlugin));
    let p = app
        .world_mut()
        .spawn(external(sequence(vec![video_track(
            "wall",
            vec![VideoSection::new(CLIP, 0.0, 1.0)],
        )])))
        .id();
    set_time(&mut app, p, 0.5);
    assert_eq!(issues(&app, p), ["seq_video_unavailable"], "no frame plugin");
    assert_eq!(shown(&mut app, p, 0), Some(30), "the session runs regardless");

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(33)))
        .add_plugins(SequencerPlugin);
    let p = app
        .world_mut()
        .spawn(external(sequence(vec![video_track(
            "wall",
            vec![VideoSection::new(CLIP, 0.0, 1.0)],
        )])))
        .id();
    set_time(&mut app, p, 0.5);
    assert_eq!(issues(&app, p), ["seq_video_unavailable"], "no media plugin");
    assert!(status(&app, p).is_empty());
    assert_eq!(app.world().get::<SequencePlayer>(p).expect("player").time(), 0.5);
}

/// The frame plugin's targets: a fullscreen layer, a mesh's material slot and a UI node each show
/// this frame's picture, and each is handed back when the section ends.
#[test]
fn the_screen_targets_show_this_frames_picture_and_hand_back() {
    use regia_media::frame_source::{FullscreenVideos, MediaFrameSource};
    use regia_render::video::binds_video;
    use regia_render::{VideoFrameSource, VideoFullscreenQueue};
    use regia_ui::video::UiVideoPlugin;

    let mut app = app_with(&[(CLIP, clip(120, 60).to_webm())]);
    app.add_message::<regia_ui::UiEvent>().add_plugins(UiVideoPlugin);
    let queue = VideoFullscreenQueue::default();
    app.insert_resource(FullscreenVideos(queue.clone()));
    let base = Arc::new(CpuMaterial::default());
    let empty = Arc::new(regia_render::CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    });
    let mesh = app.world_mut().spawn(MeshRenderer::new(empty, base.clone())).id();
    let plain = UiImage {
        id: 7,
        ..UiImage::default()
    };
    let node = app.world_mut().spawn(plain).id();
    let bare_node = app.world_mut().spawn_empty().id();
    let on = |target: VideoTarget| VideoSection {
        target,
        ..VideoSection::new(CLIP, 0.0, 1.0)
    };
    let player = external(sequence(vec![video_track(
        "wall",
        vec![
            on(VideoTarget::Screen {
                binding: "tv".into(),
                slot: VideoScreenSlot::Albedo,
            }),
            on(VideoTarget::UiImage("slate".into())),
            on(VideoTarget::UiImage("bare".into())),
            on(VideoTarget::Fullscreen {
                layer: -2,
                fit: VideoFit::Fill,
            }),
        ],
    )]));
    let bindings = SequenceBindings::default()
        .with("tv", Binding::Entity(mesh))
        .with("slate", Binding::Entity(node))
        .with("bare", Binding::Entity(bare_node));
    let p = app.world_mut().spawn((player, bindings)).id();
    app.update();
    set_time(&mut app, p, 0.5);
    assert!(issues(&app, p).is_empty(), "{:?}", issues(&app, p));
    let state = status(&app, p);
    assert_eq!(state.len(), 4);
    let source = app.world().resource::<MediaFrameSource>().clone();
    for section in &state {
        let media = section.media.expect("a session");
        let info = source.frame(media.to_bits()).expect("published this frame");
        assert_eq!(info.pts_ns, Some(500_000_000), "{section:?}");
    }
    // This frame's picture reaches the mesh's material (bound on this frame's publish, or the next
    // with the deferred insert).
    app.update();
    let tv_media = state[0].media.expect("session");
    let texture = source.texture(tv_media.to_bits()).expect("texture");
    let renderer = app.world().get::<MeshRenderer>(mesh).expect("mesh");
    assert!(binds_video(&renderer.material, &texture, VideoSlot::Albedo));
    assert_eq!(
        app.world().get::<UiVideo>(node).map(|v| v.player),
        Some(state[1].media.expect("session").to_bits())
    );
    let screens = queue.videos();
    assert_eq!((screens.len(), screens[0].layer), (1, -2));
    assert_eq!(screens[0].fit, regia_render::FullscreenFit::Fill);
    let full = app
        .world()
        .get::<VideoFullscreen>(state[3].media.expect("session"))
        .expect("fullscreen");
    assert!(!full.skippable, "the sequence owns it");

    // The sections end: every target is as it was.
    let mut longer = app
        .world()
        .get::<SequencePlayer>(p)
        .expect("player")
        .sequence()
        .clone();
    longer.markers.push(super::super::super::asset::Marker {
        name: "end".into(),
        time: 2.0,
        payload: String::new(),
    });
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .replace_sequence(longer);
    set_time(&mut app, p, 1.5);
    assert!(status(&app, p).is_empty());
    let renderer = app.world().get::<MeshRenderer>(mesh).expect("mesh");
    assert!(Arc::ptr_eq(&renderer.material, &base), "the material it had");
    assert!(app.world().get::<VideoMaterial>(mesh).is_none());
    assert_eq!(app.world().get::<UiImage>(node), Some(&plain), "the image it had");
    assert!(app.world().get::<UiVideo>(node).is_none());
    assert!(app.world().get::<UiImage>(bare_node).is_none(), "it had none");
    app.update();
    assert!(queue.videos().is_empty(), "off the screen");
    assert!(
        source.frame(tv_media.to_bits()).is_none(),
        "the session's player is gone"
    );
}

/// The session's own sound registers at the section's volume, follows it, and leaves with it.
#[test]
fn the_sections_volume_reaches_the_sound_of_its_session() {
    let mut toned = clip(120, 60);
    toned.audio = Some(ToneAudio {
        rate: 48_000,
        channels: 1,
        freq_hz: 440.0,
        amplitude: 0.5,
    });
    let mut app = app_with(&[(CLIP, toned.to_webm())]);
    app.init_resource::<MediaAudioRegistry>();
    let mut section = VideoSection::new(CLIP, 0.0, 2.0);
    section.volume_db = -6.0;
    let mut seq = sequence(vec![video_track("wall", vec![section.clone()])]);
    seq.markers.push(super::super::super::asset::Marker {
        name: "end".into(),
        time: 3.0,
        payload: String::new(),
    });
    let p = app.world_mut().spawn(external(seq.clone())).id();
    set_time(&mut app, p, 0.5);
    let feeds = |app: &App| {
        app.world()
            .resource::<MediaAudioRegistry>()
            .feeds()
            .iter()
            .map(|f| f.volume)
            .collect::<Vec<f32>>()
    };
    assert_eq!(feeds(&app).len(), 1);
    assert!((feeds(&app)[0] - volume_gain(-6.0)).abs() < 1.0e-6);
    section.volume_db = -12.0;
    let mut edited = seq;
    edited.tracks[0].keys = Channel::Video(vec![section]);
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .replace_sequence(edited);
    set_time(&mut app, p, 0.75);
    assert!((feeds(&app)[0] - volume_gain(-12.0)).abs() < 1.0e-6);
    set_time(&mut app, p, 2.5);
    assert!(feeds(&app).is_empty(), "it leaves with the section");
}

/// The strip's still: the picture the section shows at a source time.
#[test]
fn a_thumbnail_is_the_picture_of_its_source_time() {
    let mut files = MediaFiles::default();
    files.insert(CLIP, clip(120, 60).to_webm());
    let still = video_thumbnail(&files, CLIP, 0.5).expect("decodes");
    assert_eq!((still.width, still.height, still.frame), (16, 8, 30));
    assert_eq!(still.rgba.len(), 16 * 8 * 4);
    let first = video_thumbnail(&files, CLIP, 0.0).expect("decodes");
    assert_eq!(first.frame, 0);
    assert_ne!(first.rgba, still.rgba);
    let past = video_thumbnail(&files, CLIP, 99.0).expect("decodes");
    assert_eq!(past.frame, 119, "past the end: the last picture");
    assert_eq!(
        video_thumbnail(&files, "movies/gone.webm", 0.0)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("source_not_found")
    );
    assert_eq!(
        video_thumbnail(&files, "movies/clip.mp4", 0.0)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("container_unsupported")
    );
}

/// A feed that passes a real session through while its gate is open and is `Pending` while it is
/// shut: a decoder that has not caught up yet, under the test's control.
struct GatedFeed {
    session: Session,
    open: Arc<std::sync::atomic::AtomicBool>,
}

impl FrameFeed for GatedFeed {
    fn poll(&mut self, block: bool) -> FeedPoll {
        if self.open.load(Ordering::Relaxed) {
            self.session.poll(block)
        } else {
            FeedPoll::Pending
        }
    }

    fn seek(&mut self, to_ns: u64) {
        self.session.seek(to_ns);
    }

    fn seek_accurate(&mut self, to_ns: u64) {
        self.session.seek_accurate(to_ns);
    }

    fn stats(&self) -> FeedStats {
        self.session.stats()
    }
}

fn seek_bound_issues(app: &App, player: Entity) -> usize {
    app.world()
        .get::<SequencePlayer>(player)
        .expect("player")
        .issues()
        .filter(|i| matches!(i, SequenceIssue::VideoFailed { code, .. } if code == "seek_bound"))
        .count()
}

/// rc1-audio finding 7 (ADR-8831): an exact section whose session has a seek pending (a game-side
/// seek the decoder has not reached yet) raises no `seek_bound` while the seek is in flight, and
/// lands once it resolves; a real miss afterwards (the decoder cannot reach the time within the
/// budget) still raises one.
#[test]
fn an_exact_scrub_during_a_pending_seek_raises_no_seek_bound_until_it_resolves() {
    let bytes = clip(600, 60).to_webm();
    let mut app = app_with(&[(CLIP, bytes.clone())]);
    let p = app
        .world_mut()
        .spawn(external(sequence(vec![video_track(
            "wall",
            vec![VideoSection::new(CLIP, 0.0, 10.0)],
        )])))
        .id();
    set_time(&mut app, p, 0.0);
    assert!(status(&app, p)[0].exact, "a manual clock is frame-exact");
    let media = status(&app, p)[0].media.expect("a session");
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let options = SessionOptions::default();
    let session = Session::open(std::io::Cursor::new(bytes), &options, None).expect("opens");
    let info = session.info().clone();
    app.world_mut()
        .get_mut::<VideoPlayerState>(media)
        .expect("session")
        .core_mut()
        .open(
            Box::new(GatedFeed {
                session,
                open: gate.clone(),
            }),
            info,
            ClockMode::External,
            LoopMode::Off,
            false,
            0.0,
        );
    set_time(&mut app, p, 0.05);
    assert!(status(&app, p)[0].certain, "{:?}", status(&app, p));
    assert_eq!(seek_bound_issues(&app, p), 0);
    // A seek the decoder cannot reach yet.
    gate.store(false, Ordering::Relaxed);
    app.world_mut()
        .get_mut::<VideoPlayerState>(media)
        .expect("session")
        .core_mut()
        .seek_time(4.0, regia_media::player::SeekMode::Accurate)
        .expect("seekable");
    for t in [0.1, 0.15, 0.2, 0.25] {
        set_time(&mut app, p, t);
        assert_eq!(
            seek_bound_issues(&app, p),
            0,
            "no seek_bound while the seek is pending, at {t}: {:?}",
            status(&app, p)
        );
    }
    // The decoder catches up: the seek resolves and the section lands on its own time.
    gate.store(true, Ordering::Relaxed);
    let mut frames = 0;
    // Times off the frame grid (x.6 frames), so the expected picture is unambiguous.
    let mut t = 0.31;
    while !(status(&app, p)[0].certain && shown(&mut app, p, 0) == Some(ideal(t, 60.0, 600))) {
        t += 0.05;
        set_time(&mut app, p, t);
        frames += 1;
        assert!(frames < 20, "never landed: {:?}", status(&app, p));
    }
    assert_eq!(seek_bound_issues(&app, p), 0, "{:?}", status(&app, p));
    // A real miss: the decoder stalls and the clock jumps; the exact section cannot land.
    gate.store(false, Ordering::Relaxed);
    set_time(&mut app, p, 6.0);
    assert!(!status(&app, p)[0].certain);
    assert_eq!(seek_bound_issues(&app, p), 1, "a real miss is still reported");
}
