// SPDX-License-Identifier: MPL-2.0
//! Concurrency audit 11-03 (regia_audio group, findings in `regia_media`; ADR-2396): the player's
//! transport reaches its audio feed (pause holds the feed, an accurate seek drops the audio before
//! its target, the rate resamples the feed), a terminal poll answers again instead of hanging, and
//! dropping a feed stops its decode thread between packets.
//!
//! Every wait here bounds a hang only (a thread + `recv_timeout`); no test measures time.

use std::io::Cursor;
use std::time::Duration;

use regia_app::prelude::*;
use regia_audio::media_feed::{MediaAudioRegistry, attach_media_feeds};
use regia_audio::{Audio, AudioSettings, BusLayout};
use regia_media::codec::{PCM_FLOAT, RAW_VIDEO};
use regia_media::container::TrackKind;
use regia_media::mux::{MuxOptions, MuxPacket, MuxTrack, PatternClip, mux, pattern_frame};
use regia_media::session::{AudioSink, FeedPoll, FrameFeed, Session, SessionOptions, WorkerFeed};
use regia_media::{
    ClockMode, MediaFiles, MediaPlugin, RingBudget, RingFeed, SeekMode, VideoPlayer, VideoPlayerState,
};

const RATE: u32 = 48_000;
const FPS: u32 = 30;

/// The sample magnitude of video frame `k`'s audio packet (signs go `+ + - -`, so the signal has no
/// DC for the mixer to filter and keeps its level when resampled by 2).
fn level(k: u32) -> f32 {
    0.01 * (k + 1) as f32
}

/// A 16x8 raw clip at 30 fps whose mono float audio has magnitude `level(k)` in frame `k`, so
/// a test can read back which part of the clip it hears. A keyframe (and cue) every 10 frames.
fn level_clip(frames: u32) -> Vec<u8> {
    let frame_ns = 1_000_000_000 / u64::from(FPS);
    let tracks = [
        MuxTrack {
            number: 1,
            kind: TrackKind::Video,
            codec_id: RAW_VIDEO.to_string(),
            default_duration_ns: Some(frame_ns),
            language: "und".to_string(),
            default: true,
            video: Some((16, 8, Some("I420".to_string()))),
            audio: None,
        },
        MuxTrack {
            number: 2,
            kind: TrackKind::Audio,
            codec_id: PCM_FLOAT.to_string(),
            default_duration_ns: None,
            language: "eng".to_string(),
            default: true,
            video: None,
            audio: Some((f64::from(RATE), 1)),
        },
    ];
    let mut packets = Vec::new();
    for k in 0..frames {
        let ns = u64::from(k) * frame_ns;
        let pts = ((ns + 500_000) / 1_000_000 * 1_000_000) as i64;
        packets.push(MuxPacket {
            track: 1,
            pts_ns: pts,
            keyframe: k.is_multiple_of(10),
            data: pattern_frame(16, 8, k),
        });
        let start = u64::from(k) * frame_ns * u64::from(RATE) / 1_000_000_000;
        let end = u64::from(k + 1) * frame_ns * u64::from(RATE) / 1_000_000_000;
        let mut data = Vec::new();
        for i in start..end {
            let v = if (i / 2).is_multiple_of(2) {
                level(k)
            } else {
                -level(k)
            };
            data.extend_from_slice(&v.to_le_bytes());
        }
        packets.push(MuxPacket {
            track: 2,
            pts_ns: pts,
            keyframe: true,
            data,
        });
    }
    let opts = MuxOptions {
        cues: true,
        duration_ns: Some(u64::from(frames) * frame_ns),
        ..MuxOptions::default()
    };
    mux(&opts, &tracks, &packets)
}

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        buses: BusLayout::default_tree(),
        ..AudioSettings::default()
    })
}

/// An app with the level clip at `movies/level.webm`; with `mixer`, a headless mixer that
/// attaches the media feeds (as `AudioPlugin` does).
fn app(mixer: bool) -> App {
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("movies/level.webm", level_clip(120));
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .init_resource::<MediaAudioRegistry>()
        .add_plugins(MediaPlugin);
    if mixer {
        app.insert_resource(headless())
            .add_systems(Update, attach_media_feeds);
    }
    app
}

fn spawn_player(app: &mut App, rate: f32, autoplay: bool) -> Entity {
    app.world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            autoplay,
            rate,
            ..VideoPlayer::new("movies/level.webm")
        })
        .id()
}

/// Renders one 800-frame block (1/60 s at 48 kHz) of the headless mixer.
fn render(app: &mut App) -> Vec<f32> {
    let mut audio = app.world_mut().resource_mut::<Audio>();
    audio.tick(1.0 / 60.0);
    audio.render_offline(800).unwrap_or_default()
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

fn stream_frames(app: &App, e: Entity) -> u64 {
    app.world()
        .resource::<MediaAudioRegistry>()
        .stream(e.to_bits())
        .map_or(0, regia_audio::media_feed::MediaStreamHandle::frames)
}

/// Audit H2 (pause): `pause` froze the picture, but the mixer kept popping the feed, so the audio
/// already decoded played on during the pause and was missing at the resume (a lasting A/V
/// offset). Paused, the stream plays silence, takes nothing from the feed and counts no frames.
#[test]
fn a_paused_player_holds_its_audio_feed() {
    let mut app = app(true);
    let e = spawn_player(&mut app, 1.0, true);
    // Decode ahead of the output: twelve video ticks (~0.2 s of audio), two output blocks.
    for _ in 0..12 {
        app.update();
    }
    for _ in 0..2 {
        render(&mut app);
    }
    assert!(stream_frames(&app, e) > 0, "the feed is attached and playing");
    app.world_mut()
        .get_mut::<VideoPlayerState>(e)
        .expect("state")
        .core_mut()
        .pause()
        .expect("pause");
    app.update();
    let before = stream_frames(&app, e);
    let mut paused = Vec::new();
    for _ in 0..4 {
        paused.extend(render(&mut app));
        app.update();
    }
    assert!(
        rms(&paused) < 1e-4,
        "the paused player's audio kept playing: rms {}",
        rms(&paused)
    );
    assert_eq!(
        stream_frames(&app, e),
        before,
        "the paused stream counted frames (its clock ran on)"
    );
    app.world_mut()
        .get_mut::<VideoPlayerState>(e)
        .expect("state")
        .core_mut()
        .play()
        .expect("play");
    app.update();
    let resumed = render(&mut app);
    assert!(
        rms(&resumed) > 1e-3,
        "the audio held over the pause plays at the resume"
    );
}

/// Audit H2 (accurate seek): the session decodes from the cue point before the target and the
/// player discards the pictures before it, but the audio of those pictures was pushed into the
/// feed and heard. An accurate seek drops decoded audio before its target.
#[test]
fn an_accurate_seek_drops_the_audio_before_its_target() {
    for (target_s, first_frame, partial) in [(0.5, 15u32, false), (0.51, 15, true)] {
        let mut app = app(false);
        let e = spawn_player(&mut app, 1.0, false);
        app.update();
        let mut entry = app
            .world_mut()
            .resource_mut::<MediaAudioRegistry>()
            .take_pending()
            .pop()
            .expect("the clip has an audio feed");
        app.world_mut()
            .get_mut::<VideoPlayerState>(e)
            .expect("state")
            .core_mut()
            .seek_time(target_s, SeekMode::Accurate)
            .expect("seek");
        // One more picture, so the audio packet after the presented picture is decoded too.
        app.world_mut()
            .get_mut::<VideoPlayerState>(e)
            .expect("state")
            .core_mut()
            .step_frames(1)
            .expect("step");
        let mut heard = vec![0.0f32; RATE as usize];
        let n = entry.consumer.pop(&mut heard);
        heard.truncate(n);
        assert!(!heard.is_empty(), "the seek decoded audio at its target");
        // Packet times are whole milliseconds (frame 14's audio starts at 466.67 ms but is
        // stamped 467 ms), so up to one millisecond (48 samples) of the packet before the target
        // may remain; nothing earlier may.
        let sliver = heard
            .iter()
            .take_while(|s| (s.abs() - level(first_frame)).abs() >= 1e-6)
            .count();
        assert!(
            sliver < 48
                && heard[..sliver]
                    .iter()
                    .all(|s| (s.abs() - level(first_frame - 1)).abs() < 1e-6),
            "seek to {target_s} s: {sliver} samples before frame {first_frame}'s audio, the first of \
             frame {} (the cue point is frame 10)",
            (heard[0].abs() / 0.01).round() as i64 - 1
        );
        assert!(sliver < heard.len(), "frame {first_frame}'s audio is heard");
        let of_first = heard
            .iter()
            .filter(|&&s| (s.abs() - level(first_frame)).abs() < 1e-6)
            .count();
        if partial {
            // 510 ms lies 10 ms (480 samples) into frame 15's packet (500..533 ms).
            assert!(
                (1100..=1130).contains(&of_first),
                "frame 15's packet is cut at the target: {of_first} samples kept"
            );
        } else {
            assert!(of_first >= 1599, "frame 15's packet is whole: {of_first}");
        }
    }
}

/// RMS of the last output block after `updates` lockstep ticks at `rate`, one block per tick.
fn level_heard_at(rate: f32, updates: u32) -> f32 {
    let mut app = app(true);
    spawn_player(&mut app, rate, true);
    let mut last = Vec::new();
    for _ in 0..updates {
        app.update();
        last = render(&mut app);
    }
    rms(&last)
}

/// Audit M3: the feed always played at 1x, so at rate 2 the soundtrack fell behind the picture
/// (and overflowed its ring). Inside 0.5..=2.0 the feed resamples by the rate (spec 50 § 2.5);
/// outside it the audio is muted.
#[test]
fn the_playback_rate_reaches_the_audio_feed() {
    let normal = level_heard_at(1.0, 40);
    let double = level_heard_at(2.0, 40);
    assert!(normal > 0.0, "the clip is audible at rate 1");
    // The level grows with media time: rate 2 is twice as far into the clip.
    let ratio = double / normal;
    assert!(
        (1.7..=2.3).contains(&ratio),
        "rate 2 should hear twice as far into the clip: {double} vs {normal} (ratio {ratio})"
    );
    let fast_rms = level_heard_at(3.0, 20);
    assert!(
        fast_rms < 1e-4,
        "rate 3 is outside 0.5..=2.0: muted (rms {fast_rms})"
    );
}

/// Polls `feed` to its end on a helper thread, then polls once more; the second terminal poll
/// must answer (and a seek must bring pictures back). `None` when a poll never answered.
fn second_terminal_poll<F: FrameFeed + 'static>(mut feed: F) -> Option<(bool, bool)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let FeedPoll::Frame(_) = feed.poll(true) {}
        let again = matches!(feed.poll(true), FeedPoll::End);
        feed.seek(0);
        let restarted = matches!(feed.poll(true), FeedPoll::Frame(_));
        let _ = tx.send((again, restarted));
    });
    // Bounds a hang only.
    rx.recv_timeout(Duration::from_secs(60)).ok()
}

fn pattern_session() -> Session {
    let clip = PatternClip::new(16, 8, 3, 30).to_webm();
    Session::open(Cursor::new(clip), &SessionOptions::default(), None).expect("open")
}

/// Audit M1: after `End` the worker went idle and never sent again, so a second blocking poll
/// waited forever (the player only avoided it with its own latch).
#[test]
fn a_worker_feed_answers_a_second_terminal_poll() {
    let feed = WorkerFeed::spawn(pattern_session(), 4).expect("spawn");
    let outcome = second_terminal_poll(feed).expect("the second blocking poll after End never answered");
    assert_eq!(outcome, (true, true), "(End again, pictures after a seek)");
}

/// Audit M1, the ring feed: the same hang (it only escaped when the worker stopped).
#[test]
fn a_ring_feed_answers_a_second_terminal_poll() {
    let feed = RingFeed::spawn(pattern_session(), RingBudget::default()).expect("spawn");
    let outcome = second_terminal_poll(feed).expect("the second blocking poll after End never answered");
    assert_eq!(outcome, (true, true), "(End again, pictures after a seek)");
}

/// A sink that blocks in every push until the test releases it, and reports its drop (the decode
/// thread dropped its session: the thread ended).
struct GateSink {
    release: crossbeam_channel::Receiver<()>,
    dropped: crossbeam_channel::Sender<()>,
}

impl AudioSink for GateSink {
    fn push(&mut self, _pts_ns: i64, _samples: &[f32]) {
        let _ = self.release.recv();
    }

    fn flush(&mut self) {}
}

impl Drop for GateSink {
    fn drop(&mut self) {
        let _ = self.dropped.send(());
    }
}

/// One picture, then an audio-only tail of `packets` 1 ms packets.
fn audio_tail_clip(packets: u32) -> Vec<u8> {
    let tracks = [
        MuxTrack {
            number: 1,
            kind: TrackKind::Video,
            codec_id: RAW_VIDEO.to_string(),
            default_duration_ns: Some(33_333_333),
            language: "und".to_string(),
            default: true,
            video: Some((16, 8, Some("I420".to_string()))),
            audio: None,
        },
        MuxTrack {
            number: 2,
            kind: TrackKind::Audio,
            codec_id: PCM_FLOAT.to_string(),
            default_duration_ns: None,
            language: "eng".to_string(),
            default: true,
            video: None,
            audio: Some((f64::from(RATE), 1)),
        },
    ];
    let mut out = vec![MuxPacket {
        track: 1,
        pts_ns: 0,
        keyframe: true,
        data: pattern_frame(16, 8, 0),
    }];
    let samples: Vec<u8> = (0..48).flat_map(|_| 0.25f32.to_le_bytes()).collect();
    for k in 0..packets {
        out.push(MuxPacket {
            track: 2,
            pts_ns: i64::from(k) * 1_000_000,
            keyframe: true,
            data: samples.clone(),
        });
    }
    mux(&MuxOptions::default(), &tracks, &out)
}

/// Releases sink pushes one at a time until the decode thread ends or `budget` pushes went by.
/// Returns the pushes released and whether the thread ended.
fn release_until_stopped(
    release: &crossbeam_channel::Sender<()>,
    dropped: &crossbeam_channel::Receiver<()>,
    budget: u32,
) -> (u32, bool) {
    let mut released = 0;
    while released < budget {
        crossbeam_channel::select! {
            send(release, ()) -> r => {
                if r.is_err() {
                    break;
                }
                released += 1;
            }
            recv(dropped) -> _ => return (released, true),
            // Bounds a hang only.
            default(Duration::from_secs(60)) => return (released, false),
        }
    }
    (released, dropped.try_recv().is_ok())
}

fn check_drop_stops_between_packets<F: FrameFeed + 'static>(spawn: impl FnOnce(Session) -> F) {
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(0);
    let (dropped_tx, dropped_rx) = crossbeam_channel::unbounded::<()>();
    let sink = GateSink {
        release: release_rx,
        dropped: dropped_tx,
    };
    let session = Session::open(
        Cursor::new(audio_tail_clip(3000)),
        &SessionOptions::default(),
        Some(Box::new(sink)),
    )
    .expect("open");
    let mut feed = spawn(session);
    // The picture is the clip's first packet, so it arrives before any audio push.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let first = matches!(feed.poll(true), FeedPoll::Frame(_));
        let _ = tx.send(first);
        // The decode thread now sits in the audio-only tail; the drop must stop it.
        drop(feed);
    });
    // Bounds a hang only.
    let first = rx.recv_timeout(Duration::from_secs(60)).ok();
    assert_eq!(first, Some(true), "the first picture arrived");
    let (released, stopped) = release_until_stopped(&release_tx, &dropped_rx, 1000);
    drop(release_tx);
    assert!(
        stopped,
        "the decode thread kept decoding the audio tail after the drop ({released} packets)"
    );
}

/// Audit M4 (worker feed): the drop's stop was only seen between pictures, so a worker in a long
/// audio-only tail decoded all of it before the drop could join it.
#[test]
fn dropping_a_worker_feed_stops_its_thread_between_packets() {
    check_drop_stops_between_packets(|s| WorkerFeed::spawn(s, 4).expect("spawn"));
}

/// Audit M4 (ring feed).
#[test]
fn dropping_a_ring_feed_stops_its_thread_between_packets() {
    check_drop_stops_between_packets(|s| RingFeed::spawn(s, RingBudget::default()).expect("spawn"));
}

/// Spec 50 § 3.3 / test 8: when the output outruns the decoded audio, the stream plays silence and
/// counts underruns, and the player's `VideoStats::underruns` reports them.
#[test]
fn an_empty_ring_plays_silence_and_counts_underruns() {
    let mut app = app(true);
    let e = spawn_player(&mut app, 1.0, true);
    for _ in 0..3 {
        app.update();
    }
    assert!(
        app.world()
            .resource::<MediaAudioRegistry>()
            .stream(e.to_bits())
            .is_some(),
        "the feed is attached"
    );
    // About 0.33 s of output with no video ticks: the ring runs dry.
    let mut tail = Vec::new();
    for _ in 0..20 {
        tail = render(&mut app);
    }
    assert!(rms(&tail) < 1e-4, "a dry ring is silence: rms {}", rms(&tail));
    app.update();
    let underruns = app
        .world()
        .get::<VideoPlayerState>(e)
        .expect("state")
        .core()
        .stats()
        .underruns;
    assert!(underruns > 0, "the dry ring was counted");
}

// ---- v0.5.1 media runtime fixes: deep-zai-media finding 2 (resync after a decode pause) ---------

/// Every audio push as (media time, first sample), so a test can read which picture's audio
/// was sent to the mixer first.
#[derive(Clone, Default)]
struct PushLog(std::sync::Arc<std::sync::Mutex<Vec<(i64, f32)>>>);

impl PushLog {
    fn take(&self) -> Vec<(i64, f32)> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

impl AudioSink for PushLog {
    fn push(&mut self, pts_ns: i64, samples: &[f32]) {
        if let Some(&first) = samples.first() {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((pts_ns, first));
        }
    }

    fn flush(&mut self) {}
}

/// A session that answers `Pending` on every other non-blocking poll, as a decode thread that
/// is still catching up does (deterministic: no thread).
struct Stutter {
    inner: Session,
    pending_next: bool,
}

impl FrameFeed for Stutter {
    fn poll(&mut self, block: bool) -> FeedPoll {
        if !block {
            self.pending_next = !self.pending_next;
            if !self.pending_next {
                return FeedPoll::Pending;
            }
        }
        self.inner.poll(block)
    }

    fn seek(&mut self, to_ns: u64) {
        self.inner.seek(to_ns);
    }

    fn seek_accurate(&mut self, to_ns: u64) {
        self.inner.seek_accurate(to_ns);
    }

    fn stats(&self) -> regia_media::session::FeedStats {
        self.inner.stats()
    }
}

/// The level clip's picture a linear play shows at pass position `pos_ns`.
fn level_frame_at(pos_ns: i64) -> u32 {
    let frame_ns = 1_000_000_000 / u64::from(FPS);
    (0..120u32)
        .rfind(|&k| ((u64::from(k) * frame_ns + 500_000) / 1_000_000 * 1_000_000) as i64 <= pos_ns)
        .unwrap_or(0)
}

const TICK: regia_media::player::ClockInput = regia_media::player::ClockInput::Elapsed { dt_ns: 16_666_667 };

/// A `Game`-clock player on the level clip: opens and plays 5 ticks (the opening tick does not
/// advance the clock), is hidden (`PauseDecode`: the clock runs on) for 30 ticks and is shown again,
/// so the next tick puts the clock at frame 17 while the cue before it is frame 10.
fn hidden_then_shown(
    feed: impl FnOnce(Session) -> Box<dyn FrameFeed>,
    log: &PushLog,
) -> regia_media::player::PlayerCore {
    use regia_media::player::{LoopMode, PlayerCore};
    let s = Session::open(
        Cursor::new(level_clip(120)),
        &SessionOptions::default(),
        Some(Box::new(log.clone())),
    )
    .expect("opens");
    let info = s.info().clone();
    let mut core = PlayerCore::new();
    core.open(feed(s), info, ClockMode::Game, LoopMode::Off, true, 0.0);
    for _ in 0..5 {
        core.tick(TICK);
    }
    assert_eq!(core.state(), regia_media::player::PlayState::Playing);
    core.set_decode_paused(true, false);
    for _ in 0..30 {
        core.tick(TICK);
    }
    log.take();
    core.set_decode_paused(false, false);
    core
}

fn shown_frame(core: &regia_media::player::PlayerCore) -> u32 {
    regia_media::mux::pattern_index(&core.presented().expect("a picture").planes[0])
}

/// Finding 2 (audio): coming back into view re-seeks the feed to the clock. Spec 50 § 2.7 makes
/// that an accurate seek, so the audio between the cue point (frame 10) and the clock (frame 17)
/// is dropped. The keyframe seek pushed it to the mixer: the soundtrack restarted at the cue
/// point, a lasting offset of eight frames from the picture.
#[test]
fn becoming_visible_drops_the_audio_before_the_clock() {
    let log = PushLog::default();
    let mut core = hidden_then_shown(|s| Box::new(s), &log);
    core.tick(TICK);
    let pos = core.position_ns();
    let expected = level_frame_at(pos);
    assert_eq!(
        expected, 17,
        "the clock is at frame 17 ({pos} ns), eight past the cue (frame 10)"
    );
    assert_eq!(shown_frame(&core), expected, "the picture is the clock's");
    let pushes = log.take();
    let &(first_pts, first) = pushes.first().expect("the resync decoded audio");
    assert!(
        first_pts >= pos - 1_000_000,
        "the first audio after the resync is at {first_pts} ns, the clock at {pos} ns"
    );
    assert!(
        (first.abs() - level(expected)).abs() < 1e-6,
        "the first audio heard is frame {}'s, the picture is frame {expected}'s",
        (first.abs() / 0.01).round() as i64 - 1
    );
}

/// Finding 2 (picture): with a feed still catching up (`Pending` between pictures), the keyframe
/// resync presented whatever older picture had arrived (the cue point, eight frames behind the
/// clock). The accurate landing keeps the old picture until the one due at the clock is known,
/// so the first picture presented after becoming visible is the clock's.
#[test]
fn the_first_picture_after_becoming_visible_is_the_clocks() {
    let log = PushLog::default();
    let mut core = hidden_then_shown(
        |s| {
            Box::new(Stutter {
                inner: s,
                pending_next: false,
            })
        },
        &log,
    );
    let before = core.stats().frames_presented;
    let mut ticks = 0;
    while core.stats().frames_presented == before {
        core.tick(TICK);
        ticks += 1;
        assert!(ticks < 120, "no picture presented after becoming visible");
    }
    let pos = core.position_ns();
    assert_eq!(
        shown_frame(&core),
        level_frame_at(pos),
        "first picture after becoming visible (tick {ticks}, clock {pos} ns)"
    );
    assert_eq!(
        core.stats().frames_dropped,
        0,
        "pictures discarded by the resync are not drops"
    );
}

// ---- deep-zai-media finding 3: the audio ring holds the decode-ahead of a low frame rate clip --

/// Finding 3: the feed's ring held 500 ms whatever the clip, but the decode thread runs up to
/// `queue_frames` pictures (plus the one it is sending) ahead of the clock, pushing their audio.
/// At 4 fps that is 1.25 s of audio for the default queue of 4: the ring kept the oldest 500 ms
/// and silently dropped the rest, so a slideshow or time-lapse played with gaps. The ring now
/// covers the decode-ahead window.
#[test]
fn a_low_frame_rate_clip_keeps_the_audio_of_its_decode_ahead() {
    use regia_media::mux::ToneAudio;
    let mut clip = PatternClip::new(16, 8, 12, 4);
    clip.audio = Some(ToneAudio {
        rate: RATE,
        channels: 1,
        freq_hz: 440.0,
        amplitude: 0.5,
    });
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("movies/slides.webm", clip.to_webm());
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .init_resource::<MediaAudioRegistry>()
        .add_plugins(MediaPlugin);
    let e = app
        .world_mut()
        .spawn(VideoPlayer {
            clock: ClockMode::Lockstep,
            autoplay: false,
            ..VideoPlayer::new("movies/slides.webm")
        })
        .id();
    app.update();
    let mut entry = app
        .world_mut()
        .resource_mut::<MediaAudioRegistry>()
        .take_pending()
        .pop()
        .expect("the clip has an audio feed");
    // The default queue (4 pictures) plus the one being sent: five pictures, so five 250 ms
    // audio packets, decoded with nothing reading the ring (the output has not started yet).
    app.world_mut()
        .get_mut::<VideoPlayerState>(e)
        .expect("state")
        .core_mut()
        .step_frames(5)
        .expect("step");
    let decoded = 5 * (RATE as usize / 4);
    let mut heard = vec![0.0f32; 4 * RATE as usize];
    let n = entry.consumer.pop(&mut heard);
    assert!(
        n >= decoded,
        "the ring kept {n} of the {decoded} samples decoded ahead ({} ms of {} ms)",
        n * 1000 / RATE as usize,
        decoded * 1000 / RATE as usize
    );
}

// ---- v0.5.1 review M1 (ADR-7635): the PCM ring is capped by bytes ------------------------------

/// A few-KB hostile file: one 16x8 raw picture with a 10 s `DefaultDuration`, and a float PCM track
/// declaring the container's maximum rate (768 kHz) and 8 channels, with one packet of 8 frames.
fn hostile_ring_clip() -> Vec<u8> {
    let tracks = [
        MuxTrack {
            number: 1,
            kind: TrackKind::Video,
            codec_id: RAW_VIDEO.to_string(),
            default_duration_ns: Some(10_000_000_000),
            language: "und".to_string(),
            default: true,
            video: Some((16, 8, Some("I420".to_string()))),
            audio: None,
        },
        MuxTrack {
            number: 2,
            kind: TrackKind::Audio,
            codec_id: PCM_FLOAT.to_string(),
            default_duration_ns: None,
            language: "eng".to_string(),
            default: true,
            video: None,
            audio: Some((768_000.0, 8)),
        },
    ];
    let packets = [
        MuxPacket {
            track: 1,
            pts_ns: 0,
            keyframe: true,
            data: pattern_frame(16, 8, 0),
        },
        MuxPacket {
            track: 2,
            pts_ns: 0,
            keyframe: true,
            data: vec![0; 8 * 8 * 4],
        },
    ];
    let opts = MuxOptions {
        duration_ns: Some(10_000_000_000),
        ..MuxOptions::default()
    };
    mux(&opts, &tracks, &packets)
}

/// M1 (ADR-7635): the ring size comes from the file (rate, channels, video frame period), and the
/// ring allocates when the player opens. Before the byte cap this few-KB file cost 768000 x 4 x 8
/// samples of 4 bytes, about 98 MB per player and about 790 MB at the default `max_players` of 8.
/// The ring the player registers now allocates at most 8 MiB.
#[test]
fn a_hostile_audio_track_cannot_size_a_huge_ring() {
    let bytes = hostile_ring_clip();
    assert!(bytes.len() < 4096, "the hostile file is {} bytes", bytes.len());
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("movies/hostile.mkv", bytes);
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .init_resource::<MediaAudioRegistry>()
        .add_plugins(MediaPlugin);
    app.world_mut().spawn(VideoPlayer {
        clock: ClockMode::Lockstep,
        autoplay: false,
        ..VideoPlayer::new("movies/hostile.mkv")
    });
    app.update();
    let entry = app
        .world_mut()
        .resource_mut::<MediaAudioRegistry>()
        .take_pending()
        .pop()
        .expect("the 8-channel track gets an audio feed");
    assert_eq!(entry.consumer.channels(), 8);
    assert_eq!(entry.sample_rate, 768_000);
    let allocated = entry.consumer.capacity_frames() * u64::from(entry.consumer.channels()) * 4;
    assert!(
        allocated <= 8 * 1024 * 1024,
        "the player's PCM ring allocated {allocated} bytes ({} MB) for a {}-byte file",
        allocated / 1_000_000,
        hostile_ring_clip().len()
    );
}

/// ADR-7635: a real clip (48 kHz stereo at 30 fps, the default queue of 4) keeps the ring it had
/// before the cap: half a second plus six pictures of audio.
#[test]
fn a_real_clip_keeps_its_ring_size() {
    use regia_media::mux::ToneAudio;
    let mut clip = PatternClip::new(16, 8, 3, FPS);
    clip.audio = Some(ToneAudio {
        rate: RATE,
        channels: 2,
        freq_hz: 440.0,
        amplitude: 0.5,
    });
    let mut app = App::new();
    let mut files = MediaFiles::default();
    files.insert("movies/real.webm", clip.to_webm());
    app.add_plugins(MinimalPlugins)
        .insert_resource(files)
        .init_resource::<MediaAudioRegistry>()
        .add_plugins(MediaPlugin);
    app.world_mut().spawn(VideoPlayer {
        clock: ClockMode::Lockstep,
        autoplay: false,
        ..VideoPlayer::new("movies/real.webm")
    });
    app.update();
    let entry = app
        .world_mut()
        .resource_mut::<MediaAudioRegistry>()
        .take_pending()
        .pop()
        .expect("the clip has an audio feed");
    assert_eq!(entry.consumer.channels(), 2);
    // 0.5 s + 6 x 33.333 ms at 48 kHz (the frame period is 1e9 / 30 ns, integer).
    assert_eq!(entry.consumer.capacity_frames(), 24_000 + 9_599);
}
