// SPDX-License-Identifier: MPL-2.0
//! specs/50 § 8 acceptance tests that need no GPU: probe, state machine, lockstep stepping, seek,
//! loop, corrupt streams and the A/V clock. Fixtures come from the in-crate muxer (deterministic
//! pattern clips); every assertion uses fixed steps and counters, never wall clock.

use std::io::Cursor;

use regia_media::codec::{DecodedPicture, RAW_VIDEO, VideoDecoder};
use regia_media::container::{Demuxer, MkvDemuxer, Packet, TrackKind, check_extension, probe};
use regia_media::isolate::{DecodeCaps, Guarded, GuardedVideo};
use regia_media::mux::{PatternClip, ToneAudio, pattern_frame, pattern_index};
use regia_media::player::{
    ClockInput, ClockMode, LoopMode, PlayState, PlayerCore, SeekMode, VideoEvent, lockstep_audio_frames,
    lockstep_time_ns,
};
use regia_media::reference::{fnv1a64, picture_to_rgba8};
use regia_media::session::{
    AudioChoice, CollectSink, FeedPoll, FeedStats, FrameFeed, Session, SessionOptions, WorkerFeed,
};
use regia_media::{MediaError, MediaInfo};

const W: u32 = 16;
const H: u32 = 8;

fn session(bytes: Vec<u8>) -> (Session, MediaInfo) {
    let s = Session::open(Cursor::new(bytes), &SessionOptions::default(), None).expect("session opens");
    let info = s.info().clone();
    (s, info)
}

fn open_core(clip: &PatternClip, clock: ClockMode, loop_mode: LoopMode, autoplay: bool) -> PlayerCore {
    let (s, info) = session(clip.to_webm());
    let mut core = PlayerCore::new();
    core.open(Box::new(s), info, clock, loop_mode, autoplay, 0.0);
    core
}

fn shown(core: &PlayerCore) -> u32 {
    pattern_index(&core.presented().expect("a picture is presented").planes[0])
}

fn frame_hash(core: &PlayerCore) -> u64 {
    fnv1a64(&picture_to_rgba8(core.presented().expect("presented")))
}

// ---- 1. container probe -------------------------------------------------------------------------

#[test]
fn probe_reports_duration_size_rate_and_tracks() {
    let mut clip = PatternClip::new(W, H, 24, 24);
    clip.audio = Some(ToneAudio {
        rate: 48_000,
        channels: 2,
        freq_hz: 440.0,
        amplitude: 0.5,
    });
    let info = probe(Cursor::new(clip.to_webm())).expect("probe");
    assert_eq!(info.duration_ns, Some(clip.duration_ns()));
    assert_eq!(info.tracks.len(), 2);
    let v = info.video_track().expect("video");
    assert_eq!(v.codec_id, RAW_VIDEO);
    let vp = v.video.as_ref().expect("video params");
    assert_eq!((vp.width, vp.height), (W, H));
    assert!((v.frame_rate.expect("rate") - 24.0).abs() < 1e-3);
    let a = info.default_audio_track("eng").expect("audio");
    assert_eq!(a.kind, TrackKind::Audio);
    assert_eq!(a.language, "eng");
    assert_eq!(a.audio.as_ref().map(|p| p.channels), Some(2));
    assert_eq!(info.cue_count, 5, "one cue per keyframe cluster (every 5 frames)");
    assert!(info.reject_reason().is_none());
}

#[test]
fn h264_first_video_track_is_codec_patented() {
    let mut clip = PatternClip::new(W, H, 3, 24);
    clip.video_codec = "V_MPEG4/ISO/AVC".into();
    let info = probe(Cursor::new(clip.to_webm())).expect("probe still reports");
    assert_eq!(info.reject_reason().map(|e| e.code()), Some("codec_patented"));
    let err =
        Session::open(Cursor::new(clip.to_webm()), &SessionOptions::default(), None).expect_err("refused");
    assert_eq!(err.code(), "codec_patented");
    assert!(err.to_string().contains("re-encode to AV1 in WebM"));
}

#[test]
fn mp4_is_container_unsupported() {
    let mut mp4 = vec![0, 0, 0, 0x18];
    mp4.extend_from_slice(b"ftypisom");
    mp4.resize(64, 0);
    assert_eq!(
        probe(Cursor::new(mp4)).map(|_| ()).map_err(|e| e.code()),
        Err("container_unsupported")
    );
    assert_eq!(
        check_extension("movies/intro.mp4").map_err(|e| e.code()),
        Err("container_unsupported")
    );
    assert!(check_extension("movies/intro.webm").is_ok());
}

// With the `av1` feature AV1 opens (tests/codecs.rs); without it the rule refuses it with a build hint.
#[cfg(not(feature = "av1"))]
#[test]
fn av1_without_a_decoder_is_codec_unsupported() {
    let mut clip = PatternClip::new(W, H, 3, 24);
    clip.video_codec = "V_AV1".into();
    let err =
        Session::open(Cursor::new(clip.to_webm()), &SessionOptions::default(), None).expect_err("no decoder");
    assert_eq!(err.code(), "codec_unsupported");
}

#[test]
fn vp9_has_no_decoder_in_any_build() {
    let mut clip = PatternClip::new(W, H, 3, 24);
    clip.video_codec = "V_VP9".into();
    let err =
        Session::open(Cursor::new(clip.to_webm()), &SessionOptions::default(), None).expect_err("no decoder");
    assert_eq!(err.code(), "codec_unsupported");
}

#[test]
fn default_audio_matches_bcp47_and_iso639_and_skips_und() {
    use regia_media::codec::CodecVerdict;
    use regia_media::container::{ContainerKind, TrackInfo, languages_match};
    assert!(languages_match("en-US", "eng"));
    assert!(languages_match("de", "ger"));
    assert!(languages_match("fr-FR", "fra"));
    assert!(languages_match("fr", "fre"));
    assert!(!languages_match("und", "eng"));
    assert!(!languages_match("und", "und"));
    assert!(!languages_match("en", "de"));
    let audio = |index: u32, language: &str, default: bool| TrackInfo {
        number: u64::from(index + 1),
        index,
        kind: TrackKind::Audio,
        codec_id: "A_PCM/FLOAT/IEEE".into(),
        codec_private: Vec::new(),
        language: language.into(),
        default,
        default_duration_ns: None,
        frame_rate: None,
        video: None,
        audio: None,
        verdict: CodecVerdict::Supported,
    };
    let info = MediaInfo {
        container: ContainerKind::Webm,
        duration_ns: None,
        timestamp_scale: 1_000_000,
        tracks: vec![audio(0, "deu", false), audio(1, "fra", false)],
        cue_count: 0,
        markers: Vec::new(),
    };
    assert_eq!(info.default_audio_track("fr-FR").expect("fra").index, 1);
    assert_eq!(info.default_audio_track("eng").expect("first").index, 0);
    let flagged = MediaInfo {
        tracks: vec![audio(0, "deu", false), audio(1, "fra", true)],
        ..info
    };
    assert_eq!(flagged.default_audio_track("eng").expect("default flag").index, 1);
}

// ---- 2. state machine ---------------------------------------------------------------------------

#[test]
fn state_machine_rows() {
    let clip = PatternClip::new(W, H, 4, 30);
    let mut core = PlayerCore::new();
    assert_eq!(
        core.play().map_err(|e| e.code()),
        Err("bad_state"),
        "play while Closed"
    );
    // Closed -> Opening -> Ready (lockstep finishes opening in the call).
    let (s, info) = session(clip.to_webm());
    core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, false, 0.0);
    assert_eq!(core.state(), PlayState::Ready);
    assert_eq!(core.drain_events(), vec![VideoEvent::Opened]);
    // Ready -> Playing -> Paused -> Playing.
    core.play().expect("play");
    assert_eq!(core.state(), PlayState::Playing);
    core.pause().expect("pause");
    assert_eq!(core.state(), PlayState::Paused);
    core.play().expect("resume");
    assert_eq!(core.drain_events(), vec![VideoEvent::Paused, VideoEvent::Resumed]);
    // Rate rules.
    assert_eq!(core.set_rate(5.0).map_err(|e| e.code()), Err("rate_unsupported"));
    assert_eq!(core.set_rate(-1.0).map_err(|e| e.code()), Err("rate_unsupported"));
    core.set_rate(0.0).expect("zero pauses");
    assert_eq!(core.state(), PlayState::Paused);
    core.set_rate(1.0).expect("rate");
    core.play().expect("play");
    core.drain_events();
    // Playing -> Ended exactly once.
    for _ in 0..10 {
        core.tick(ClockInput::Step { dt_ns: clip.frame_ns });
    }
    assert_eq!(core.state(), PlayState::Ended);
    let ended = core
        .drain_events()
        .iter()
        .filter(|e| matches!(e, VideoEvent::Ended { skipped: false }))
        .count();
    assert_eq!(ended, 1);
    assert_eq!(shown(&core), 3, "the texture keeps the last frame");
    // Ended -> play restarts from 0.
    core.play().expect("replay");
    assert_eq!(core.state(), PlayState::Playing);
    assert_eq!(shown(&core), 0);
    // any -> Closed clears the picture.
    core.close();
    assert_eq!(core.state(), PlayState::Closed);
    assert!(core.presented().is_none());
    assert!(core.drain_events().contains(&VideoEvent::Closed));
}

#[test]
fn play_during_opening_is_remembered() {
    let clip = PatternClip::new(W, H, 4, 30);
    let (s, info) = session(clip.to_webm());
    let mut core = PlayerCore::new();
    // Game clock: opening completes on the first tick.
    core.open(Box::new(s), info, ClockMode::Game, LoopMode::Off, false, 0.0);
    assert_eq!(core.state(), PlayState::Opening);
    core.play().expect("queued");
    core.tick(ClockInput::Elapsed { dt_ns: 0 });
    assert_eq!(core.state(), PlayState::Playing);
}

#[test]
fn open_failure_is_failed_until_reopened() {
    let mut core = PlayerCore::new();
    core.fail_open(MediaError::invalid("bad"));
    assert_eq!(core.state(), PlayState::Failed);
    assert_eq!(
        core.drain_events(),
        vec![VideoEvent::OpenFailed {
            code: "container_invalid".into()
        }]
    );
    assert_eq!(core.play().map_err(|e| e.code()), Err("bad_state"));
    let clip = PatternClip::new(W, H, 2, 30);
    let (s, info) = session(clip.to_webm());
    core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, true, 0.0);
    assert_eq!(
        core.state(),
        PlayState::Playing,
        "Failed -> Opening -> Ready -> autoplay"
    );
}

#[test]
fn one_frame_clip_ends_after_its_frame() {
    let clip = PatternClip::new(W, H, 1, 30);
    let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, true);
    core.tick(ClockInput::Step { dt_ns: clip.frame_ns });
    assert_eq!(core.state(), PlayState::Playing);
    core.tick(ClockInput::Step { dt_ns: clip.frame_ns });
    assert_eq!(core.state(), PlayState::Ended);
}

#[test]
fn skip_reports_skipped_and_closes() {
    let clip = PatternClip::new(W, H, 4, 30);
    let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, true);
    core.drain_events();
    assert!(core.skip());
    assert_eq!(
        core.drain_events(),
        vec![VideoEvent::Ended { skipped: true }, VideoEvent::Closed]
    );
}

// ---- 3. lockstep stepping -----------------------------------------------------------------------

#[test]
fn lockstep_24fps_at_60hz_follows_the_integer_rule() {
    let clip = PatternClip::new(W, H, 48, 24);
    let dt = 16_666_667u64;
    let run = || {
        let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, true);
        let mut indices = Vec::new();
        let mut hashes = Vec::new();
        for _ in 0..120 {
            core.tick(ClockInput::Step { dt_ns: dt });
            indices.push(shown(&core));
            hashes.push(frame_hash(&core));
        }
        (indices, hashes, core.stats())
    };
    let (indices, hashes, stats) = run();
    let expected: Vec<u32> = (0..120u64)
        .map(|n| {
            let t = lockstep_time_ns(0, n, dt, 1000);
            (0..clip.frames)
                .rev()
                .find(|k| clip.frame_pts_ns(*k) <= t)
                .unwrap_or(0)
        })
        .collect();
    assert_eq!(indices, expected);
    assert_eq!(stats.frames_dropped, 0);
    let (again, hashes_again, _) = run();
    assert_eq!(again, indices);
    assert_eq!(hashes_again, hashes, "repeat runs are bit-identical");
}

#[test]
fn lockstep_audio_frames_sum_exactly() {
    let total: u64 = (0..600)
        .map(|n| lockstep_audio_frames(48_000, 16_666_667, 1000, n))
        .sum();
    // floor(600 * 48000 * 0.016666667) = 480000 (+ 0.0096 rounding of the integer step)
    assert_eq!(total, (600u128 * 48_000 * 16_666_667 / 1_000_000_000) as u64);
    let half: u64 = (0..10)
        .map(|n| lockstep_audio_frames(44_100, 10_000_000, 500, n))
        .sum();
    assert_eq!(half, 2205);
}

// ---- 4. seek ------------------------------------------------------------------------------------

#[test]
fn accurate_seek_matches_decoding_from_zero() {
    let clip = PatternClip::new(W, H, 20, 25);
    let mut reference = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, false);
    reference.step_frames(13).expect("step");
    assert_eq!(shown(&reference), 13);
    let want = frame_hash(&reference);

    let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, false);
    core.seek_frame(13, SeekMode::Accurate).expect("seek");
    assert_eq!(shown(&core), 13);
    assert_eq!(frame_hash(&core), want);
    assert_eq!(core.state(), PlayState::Paused);
    assert!(
        core.drain_events()
            .iter()
            .any(|e| matches!(e, VideoEvent::Seeked { .. }))
    );
}

/// A frame count past `u64::MAX / frame_ns` used to overflow: a debug build aborted on it and a
/// release build seeked to a wrapped time. The wire saturates `video.seek { frame: 1e19 }` to this.
/// A declared duration refuses it earlier, so the clip here has a period and no duration.
#[test]
fn a_frame_seek_that_overflows_nanoseconds_is_out_of_range() {
    use regia_media::mux::{MuxOptions, MuxPacket, MuxTrack, mux};
    let frame_ns = 41_666_667;
    let video = MuxTrack {
        number: 1,
        kind: TrackKind::Video,
        codec_id: RAW_VIDEO.to_string(),
        default_duration_ns: Some(frame_ns),
        language: "und".to_string(),
        default: true,
        video: Some((W, H, Some("I420".to_string()))),
        audio: None,
    };
    let packets: Vec<MuxPacket> = (0..3)
        .map(|k| MuxPacket {
            track: 1,
            pts_ns: i64::from(k) * 42_000_000,
            keyframe: true,
            data: vec![0x80; (W * H * 3 / 2) as usize],
        })
        .collect();
    let options = MuxOptions {
        duration_ns: None,
        ..MuxOptions::default()
    };
    let (s, info) = session(mux(&options, &[video], &packets));
    assert_eq!(info.duration_ns, None, "the fixture must not declare a duration");
    let mut core = PlayerCore::new();
    core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, false, 0.0);
    assert_eq!(
        core.seek_frame(u64::MAX, SeekMode::Accurate)
            .map_err(|e| e.code()),
        Err("seek_out_of_range"),
    );
    assert_eq!(
        core.seek_frame(u64::MAX / frame_ns + 1, SeekMode::Accurate)
            .map_err(|e| e.code()),
        Err("seek_out_of_range"),
    );
    assert!(core.seek_frame(1, SeekMode::Accurate).is_ok());
}

/// A period past `i64::MAX` nanoseconds went negative in every cast that reads it (the `frame_index`
/// rounding, the held-past-period test); such a value reads as "no declared period".
#[test]
fn a_frame_period_past_i64_counts_as_not_declared() {
    use regia_media::mux::{MuxOptions, MuxPacket, MuxTrack, mux};
    let track = |d: u64| MuxTrack {
        number: 1,
        kind: TrackKind::Video,
        codec_id: RAW_VIDEO.to_string(),
        default_duration_ns: Some(d),
        language: "und".to_string(),
        default: true,
        video: Some((W, H, Some("I420".to_string()))),
        audio: None,
    };
    let packet = MuxPacket {
        track: 1,
        pts_ns: 0,
        keyframe: true,
        data: vec![0x80; (W * H * 3 / 2) as usize],
    };
    let period = |d: u64| {
        let bytes = mux(&MuxOptions::default(), &[track(d)], std::slice::from_ref(&packet));
        probe(Cursor::new(bytes))
            .expect("probe")
            .video_track()
            .and_then(|t| t.default_duration_ns)
    };
    assert_eq!(period(1 << 40), Some(1 << 40));
    assert_eq!(period(i64::MAX as u64), Some(i64::MAX as u64));
    assert_eq!(period(1 << 63), None);
    assert_eq!(period(u64::MAX), None);
}

#[test]
fn keyframe_seek_lands_on_the_cue_point() {
    let clip = PatternClip::new(W, H, 20, 25); // cues every 5 frames
    let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, false);
    core.seek_frame(13, SeekMode::Keyframe).expect("seek");
    assert_eq!(shown(&core), 10);
}

#[test]
fn seek_targets_clamp_and_frame_seeks_past_the_end_fail() {
    let clip = PatternClip::new(W, H, 10, 25);
    let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, false);
    core.seek_time(99.0, SeekMode::Accurate).expect("clamped");
    assert_eq!(shown(&core), 9);
    core.seek_time(-3.0, SeekMode::Accurate).expect("clamped");
    assert_eq!(shown(&core), 0);
    assert_eq!(
        core.seek_frame(10, SeekMode::Accurate).map_err(|e| e.code()),
        Err("seek_out_of_range")
    );
}

#[test]
fn a_seek_during_a_seek_keeps_only_the_last_target() {
    let clip = PatternClip::new(W, H, 20, 25);
    let mut core = open_core(&clip, ClockMode::Game, LoopMode::Off, false);
    core.tick(ClockInput::Elapsed { dt_ns: 0 }); // finish opening
    core.drain_events();
    core.seek_frame(4, SeekMode::Accurate).expect("seek");
    core.seek_frame(17, SeekMode::Accurate).expect("seek");
    assert_eq!(core.state(), PlayState::Seeking);
    core.tick(ClockInput::Elapsed { dt_ns: 0 });
    assert_eq!(shown(&core), 17);
    let seeks = core
        .drain_events()
        .iter()
        .filter(|e| matches!(e, VideoEvent::Seeked { .. }))
        .count();
    assert_eq!(seeks, 1);
}

#[test]
fn seek_without_cues_scans_from_the_start() {
    let mut clip = PatternClip::new(W, H, 12, 25);
    clip.cues = false;
    let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, false);
    core.seek_frame(7, SeekMode::Accurate).expect("seek");
    assert_eq!(shown(&core), 7);
}

// ---- 5. loop ------------------------------------------------------------------------------------

/// A session on the time its driver writes, past opening.
fn open_external(clip: &PatternClip, autoplay: bool) -> PlayerCore {
    let mut core = open_core(clip, ClockMode::External, LoopMode::Off, autoplay);
    core.tick(ClockInput::Time { t_ns: 0 });
    core
}

/// What a linear play shows at `t`: the index of the last picture with a presentation time at or
/// before it (the first picture before any of them).
fn linear_index(clip: &PatternClip, t: u64) -> u32 {
    (0..clip.frames)
        .rfind(|&k| clip.frame_pts_ns(k) <= t as i64)
        .unwrap_or(0)
}

/// ADR-2862, the sequencer's seek: every order of visits lands on the same picture a linear play
/// shows, whether the picture is before, after or between keyframes.
#[test]
fn show_at_lands_where_a_linear_play_is_in_any_order_of_visits() {
    for cues in [true, false] {
        let mut clip = PatternClip::new(W, H, 40, 30);
        clip.cues = cues;
        let times: Vec<u64> = (0..120).map(|i| i * 11_111_111).collect();
        let mut core = open_external(&clip, false);
        for &t in &times {
            assert!(core.show_at(t, true, u32::MAX), "forward at {t}");
            assert_eq!(
                shown(&core),
                linear_index(&clip, t),
                "forward at {t}, cues {cues}"
            );
        }
        for &t in times.iter().rev() {
            assert!(core.show_at(t, true, u32::MAX), "backward at {t}");
            assert_eq!(
                shown(&core),
                linear_index(&clip, t),
                "backward at {t}, cues {cues}"
            );
        }
        // Scattered, with the same picture hash as a linear play's.
        let mut reference = open_external(&clip, false);
        for i in 0..200u64 {
            let t = (i * 7_919 * 104_729_417) % clip.duration_ns();
            assert!(core.show_at(t, true, u32::MAX));
            reference.reposition(0);
            assert!(reference.show_at(t, true, u32::MAX));
            assert_eq!(shown(&core), linear_index(&clip, t), "scattered at {t}");
            assert_eq!(frame_hash(&core), frame_hash(&reference), "scattered at {t}");
        }
        assert_eq!(
            core.stats().frames_dropped,
            0,
            "a blocking driver never counts a drop"
        );
    }
}

/// Records every accurate-seek target the player asks of its feed.
struct RecordingSeek(Session, std::sync::Arc<std::sync::Mutex<Vec<u64>>>);

impl FrameFeed for RecordingSeek {
    fn poll(&mut self, block: bool) -> FeedPoll {
        self.0.poll(block)
    }

    fn seek(&mut self, to_ns: u64) {
        self.0.seek(to_ns);
    }

    fn seek_accurate(&mut self, to_ns: u64) {
        self.1.lock().expect("seek log").push(to_ns);
        self.0.seek_accurate(to_ns);
    }

    fn stats(&self) -> FeedStats {
        self.0.stats()
    }
}

/// A driver-supplied time above the player's own clamp (`i64::MAX / 4`) must reach the feed
/// clamped: the stored landing target and the feed's seek are one operation and must agree.
#[test]
fn reposition_seeks_the_feed_with_the_clamped_time() {
    let clip = PatternClip::new(W, H, 6, 30);
    let (s, info) = session(clip.to_webm());
    let seeks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut core = PlayerCore::new();
    core.open(
        Box::new(RecordingSeek(s, seeks.clone())),
        info,
        ClockMode::External,
        LoopMode::Off,
        false,
        0.0,
    );
    core.tick(ClockInput::Time { t_ns: 0 });
    core.reposition(i64::MAX as u64 / 4 + 1);
    assert_eq!(
        *seeks.lock().expect("seek log"),
        vec![i64::MAX as u64 / 4],
        "the feed is asked to seek to the same clamped time the player stores"
    );
}

/// rc1-audio finding 6 (ADR-8831): a driver's reposition during a pending seek takes the seek over.
/// The player lands on the reposition's time only: the older seek target never reaches the feed
/// afterwards, the picture does not jump back to it on the next tick, and a waiter still hears one
/// `Seeked`.
#[test]
fn a_reposition_during_a_pending_seek_lands_on_the_newest_time_only() {
    let clip = PatternClip::new(W, H, 40, 30);
    let (s, info) = session(clip.to_webm());
    let seeks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut core = PlayerCore::new();
    core.open(
        Box::new(RecordingSeek(s, seeks.clone())),
        info,
        ClockMode::External,
        LoopMode::Off,
        false,
        0.0,
    );
    core.tick(ClockInput::Time { t_ns: 0 });
    core.drain_events();
    // A game-side seek to frame 30 is pending (real time: it runs on the next tick)...
    core.seek_frame(30, SeekMode::Accurate).expect("seek");
    assert_eq!(core.state(), PlayState::Seeking);
    // ...when the driver moves the time to 0.2 s.
    let newest = 200_000_000u64;
    core.reposition(newest);
    seeks.lock().expect("seek log").clear();
    assert!(
        core.show_at(newest, true, u32::MAX),
        "the reposition's time is shown at once"
    );
    assert_eq!(shown(&core), linear_index(&clip, newest));
    core.tick(ClockInput::Time { t_ns: newest });
    assert_eq!(
        shown(&core),
        linear_index(&clip, newest),
        "the superseded seek does not land on a later tick"
    );
    assert!(
        seeks.lock().expect("seek log").is_empty(),
        "the old target never reached the feed after the reposition"
    );
    assert_ne!(core.state(), PlayState::Seeking);
    let seeked = core
        .drain_events()
        .iter()
        .filter(|e| matches!(e, VideoEvent::Seeked { .. }))
        .count();
    assert_eq!(seeked, 1, "the game's seek still completes once");
}

/// The decode thread delivers the same pictures as the blocking session; without blocking the
/// picture converges and the caller is told when it is certain.
#[test]
fn show_at_on_the_worker_feed_matches_and_never_blocks_the_caller() {
    let clip = PatternClip::new(W, H, 40, 30);
    let (s, info) = session(clip.to_webm());
    let mut core = PlayerCore::new();
    core.open(
        Box::new(WorkerFeed::spawn(s, 4).expect("thread")),
        info,
        ClockMode::External,
        LoopMode::Off,
        false,
        0.0,
    );
    while core.state() == PlayState::Opening {
        core.tick(ClockInput::Time { t_ns: 0 });
    }
    for t in [900_000_000u64, 100_000_000, 1_200_000_000, 0, 600_000_000] {
        assert!(core.show_at(t, true, u32::MAX));
        assert_eq!(shown(&core), linear_index(&clip, t), "blocking at {t}");
    }
    let t = 300_000_000;
    let mut polls = 0u32;
    while !core.show_at(t, false, 256) {
        polls += 1;
        assert!(polls < 50_000_000, "the worker never delivered");
        std::thread::yield_now();
    }
    assert_eq!(shown(&core), linear_index(&clip, t));
}

/// `budget` bounds what one call decodes; the next calls carry on from there and land exactly.
#[test]
fn show_at_is_bounded_by_its_budget_and_carries_on() {
    let mut clip = PatternClip::new(W, H, 60, 30);
    clip.cues = false;
    let mut core = open_external(&clip, false);
    let t = clip.frame_pts_ns(50) as u64;
    let mut calls = 0;
    while !core.show_at(t, true, 8) {
        calls += 1;
        assert!(calls < 100, "never landed");
    }
    assert_eq!(
        calls, 6,
        "50 pictures at 8 a call, then the last one and its successor"
    );
    assert_eq!(shown(&core), 50);
    // A time before the picture, with no cues: from the start of the file, bounded the same way.
    core.reposition(clip.frame_pts_ns(40) as u64);
    let t = clip.frame_pts_ns(40) as u64;
    let mut calls = 0;
    while !core.show_at(t, true, 16) {
        calls += 1;
        assert!(calls < 100, "never landed");
    }
    assert_eq!(calls, 2);
    assert_eq!(shown(&core), 40);
}

/// Past the end the last picture holds and the player finishes; a time inside the clip brings it
/// back, paused, and it can play again.
#[test]
fn show_at_comes_back_from_the_end() {
    let clip = PatternClip::new(W, H, 10, 30);
    let mut core = open_external(&clip, true);
    let end = clip.duration_ns() + 1_000_000;
    assert!(core.show_at(end, true, u32::MAX));
    assert_eq!(shown(&core), 9);
    core.tick(ClockInput::Time { t_ns: end });
    assert_eq!(core.state(), PlayState::Ended);
    assert!(
        core.show_at(end + 40_000_000, true, u32::MAX),
        "still past the end"
    );
    assert_eq!(core.state(), PlayState::Ended);
    assert!(core.show_at(clip.frame_pts_ns(2) as u64, true, u32::MAX));
    assert_eq!(shown(&core), 2);
    assert_eq!(core.state(), PlayState::Paused);
    core.play().expect("plays again");
    assert_eq!(core.state(), PlayState::Playing);
    assert!(core.show_at(clip.frame_pts_ns(9) as u64, true, u32::MAX));
    assert_eq!(shown(&core), 9);
}

/// The clip opens late relative to a time before its first picture: the first picture still shows
/// after a rewind, like at open.
#[test]
fn show_at_before_the_first_picture_shows_the_first_picture() {
    let clip = PatternClip::new(W, H, 10, 30);
    let mut core = open_external(&clip, false);
    assert!(core.show_at(clip.frame_pts_ns(5) as u64, true, u32::MAX));
    core.reposition(0);
    assert!(core.show_at(0, true, u32::MAX));
    assert_eq!(shown(&core), 0);
    // Closed and failed players refuse.
    let mut closed = PlayerCore::new();
    assert!(!closed.show_at(0, true, u32::MAX));
    closed.reposition(0);
    assert_eq!(closed.state(), PlayState::Closed);
}

/// A real-time driver that falls behind counts the pictures it skipped; a reposition does not.
#[test]
fn show_at_counts_late_pictures_as_drops_only_when_it_does_not_block() {
    let clip = PatternClip::new(W, H, 20, 30);
    let mut core = open_external(&clip, false);
    assert!(core.show_at(clip.frame_pts_ns(5) as u64, false, 256));
    assert_eq!(
        core.stats().frames_dropped,
        4,
        "pictures 1 to 4 were due and skipped"
    );
    core.reposition(0);
    assert!(core.show_at(clip.frame_pts_ns(3) as u64, false, 256));
    assert_eq!(shown(&core), 3);
    assert_eq!(
        core.stats().frames_dropped,
        4,
        "a reposition's walk is not a drop"
    );
}

#[test]
fn loop_sequence_wraps_seamlessly() {
    let clip = PatternClip::new(W, H, 10, 25);
    let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Loop, true);
    let mut seen = Vec::new();
    for _ in 0..25 {
        core.tick(ClockInput::Step { dt_ns: clip.frame_ns });
        seen.push(shown(&core));
    }
    let expected: Vec<u32> = (0..10).chain(0..10).chain(0..5).collect();
    assert_eq!(seen, expected);
    let events = core.drain_events();
    let loops: Vec<u32> = events
        .iter()
        .filter_map(|e| {
            if let VideoEvent::Looped { count } = e {
                Some(*count)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(loops, vec![1, 2]);
    assert!(!events.contains(&VideoEvent::BufferingStarted));
}

#[test]
fn loop_count_two_ends_after_the_second_play() {
    let clip = PatternClip::new(W, H, 10, 25);
    let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Count(2), true);
    let mut seen = Vec::new();
    for _ in 0..25 {
        core.tick(ClockInput::Step { dt_ns: clip.frame_ns });
        seen.push(shown(&core));
        if core.state() == PlayState::Ended {
            break;
        }
    }
    assert_eq!(seen.len(), 21, "20 frames then the end tick");
    assert_eq!(&seen[..20], &(0..10).chain(0..10).collect::<Vec<_>>()[..]);
    let events = core.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, VideoEvent::Looped { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, VideoEvent::Ended { .. }))
            .count(),
        1
    );
}

// ---- 7. A/V clock -------------------------------------------------------------------------------

#[test]
fn audio_clock_drives_and_stalls_video() {
    let clip = PatternClip::new(W, H, 30, 30);
    let mut core = open_core(&clip, ClockMode::Audio, LoopMode::Off, true);
    core.tick(ClockInput::Audio {
        frames: 0,
        sample_rate: 48_000,
    }); // opening
    let mut frames = 0u64;
    for _ in 0..10 {
        frames += 1600; // one video frame period at 48 kHz
        core.tick(ClockInput::Audio {
            frames,
            sample_rate: 48_000,
        });
    }
    let at = shown(&core);
    // Within one video frame period of the audio clock.
    let audio_t = frames as f64 / 48_000.0;
    assert!(
        (core.position() - audio_t).abs() <= 1.0 / 30.0 + 1e-9,
        "{} vs {audio_t}",
        core.position()
    );
    // Stall: no frames consumed, presentation stops.
    for _ in 0..5 {
        core.tick(ClockInput::Audio {
            frames,
            sample_rate: 48_000,
        });
    }
    assert_eq!(shown(&core), at);
}

/// ADR-10105: the gameplay pause holds an audio-clocked player while the mixer clock runs on (the
/// global audio clock does not stop for a pause), and the release continues from the held
/// position instead of jumping by the paused time.
#[test]
fn the_gameplay_pause_holds_an_audio_clocked_player_in_place() {
    let clip = PatternClip::new(W, H, 90, 30);
    let mut core = open_core(&clip, ClockMode::Audio, LoopMode::Off, true);
    let tick = |core: &mut PlayerCore, frames: u64| {
        core.tick(ClockInput::Audio {
            frames,
            sample_rate: 48_000,
        });
    };
    tick(&mut core, 0); // opening
    let mut frames = 0u64;
    for _ in 0..10 {
        frames += 1600;
        tick(&mut core, frames);
    }
    let (at_frame, at_hash, at_pos) = (shown(&core), frame_hash(&core), core.position());
    core.set_game_held(true);
    // Five seconds of mixer frames over 300 paused ticks.
    for _ in 0..300 {
        frames += 800;
        tick(&mut core, frames);
        assert_eq!(shown(&core), at_frame, "held on its picture");
    }
    assert_eq!(frame_hash(&core), at_hash, "the same picture, byte for byte");
    assert_eq!(core.position(), at_pos, "the position did not move");
    core.set_game_held(false);
    frames += 1600;
    tick(&mut core, frames);
    frames += 1600;
    tick(&mut core, frames);
    let after = core.position();
    assert!(
        after > at_pos && after - at_pos <= 2.0 * 1600.0 / 48_000.0 + 1e-9,
        "continues from the held position: {at_pos} -> {after}"
    );
}

#[test]
fn audio_is_decoded_into_the_sink() {
    let mut clip = PatternClip::new(W, H, 6, 30);
    clip.audio = Some(ToneAudio {
        rate: 48_000,
        channels: 1,
        freq_hz: 1000.0,
        amplitude: 0.25,
    });
    let mut s = Session::open(
        Cursor::new(clip.to_webm()),
        &SessionOptions::default(),
        Some(Box::new(CollectSink::default())),
    )
    .expect("opens");
    assert_eq!(s.audio_format(), Some((1, 48_000)));
    let mut n = 0;
    while let regia_media::session::FeedPoll::Frame(_) = s.next_picture() {
        n += 1;
    }
    assert_eq!(n, 6);
    let none = Session::open(
        Cursor::new(clip.to_webm()),
        &SessionOptions {
            audio: AudioChoice::None,
            ..SessionOptions::default()
        },
        None,
    )
    .expect("opens");
    assert!(!none.has_audio());
}

// ---- 9. corrupt streams -------------------------------------------------------------------------

/// A decoder that refuses its first packet and takes the rest.
struct FailsOnce(bool);

impl VideoDecoder for FailsOnce {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<DecodedPicture>, MediaError> {
        if std::mem::replace(&mut self.0, false) {
            return Err(MediaError::DecodeFailed("bad".into()));
        }
        let _ = packet;
        Ok(Vec::new())
    }
    fn flush(&mut self) {}
    fn name(&self) -> &'static str {
        "fails-once"
    }
}

fn packet(pts_ns: i64, keyframe: bool) -> Packet {
    Packet {
        track: 1,
        pts_ns,
        duration_ns: None,
        keyframe,
        data: vec![0; 4],
    }
}

/// Media audit 5 (a): after one rejected packet a stream with no more keyframes fails instead of
/// freezing with `frames_corrupt` stuck at 1. A normal keyframe interval is left alone.
#[test]
fn a_stream_with_no_keyframe_left_fails_instead_of_freezing() {
    let caps = DecodeCaps::DESKTOP;
    let step = 1_000_000_000i64;
    let mut video = GuardedVideo::new(Box::new(FailsOnce(true)), caps);
    assert!(matches!(video.decode(&packet(0, true)), Ok(Guarded::Dropped)));
    let mut failed_at = None;
    for s in 1..=60 {
        match video.decode(&packet(s * step, false)) {
            Ok(Guarded::Dropped) => {}
            Err(e) => {
                assert_eq!(e.code(), "decode_failed");
                failed_at = Some(s);
                break;
            }
            Ok(Guarded::Ok(_)) => panic!("a packet after the reject was decoded"),
        }
    }
    let wait_s = (caps.max_keyframe_wait_ns / 1_000_000_000) as i64;
    assert_eq!(
        failed_at,
        Some(wait_s + 1),
        "the first dropped packet starts the wait"
    );

    // A keyframe in time ends the wait, and the next wait starts over.
    let mut video = GuardedVideo::new(Box::new(FailsOnce(true)), caps);
    assert!(matches!(video.decode(&packet(0, true)), Ok(Guarded::Dropped)));
    for s in 1..=wait_s {
        assert!(matches!(
            video.decode(&packet(s * step, false)),
            Ok(Guarded::Dropped)
        ));
    }
    assert!(matches!(
        video.decode(&packet(wait_s * step, true)),
        Ok(Guarded::Ok(_))
    ));
}

/// Media audit 5 (b): a seek with nothing decodable after it fails with an event instead of staying
/// in `Seeking` forever.
#[test]
fn a_seek_past_the_last_keyframe_fails() {
    // Cues every 5 frames but one keyframe only: the demuxer drops everything after the cue.
    let mut clip = PatternClip::new(W, H, 20, 25);
    clip.keyframe_every = 5;
    let mut bytes = clip.to_webm();
    clear_keyframe_flags_after_first(&mut bytes, &clip);
    let mut core = PlayerCore::new();
    let (s, info) = session(bytes);
    core.open(Box::new(s), info, ClockMode::Game, LoopMode::Off, false, 0.0);
    core.tick(ClockInput::Elapsed { dt_ns: 0 });
    core.drain_events();
    core.seek_frame(13, SeekMode::Accurate).expect("seek");
    for _ in 0..8 {
        core.tick(ClockInput::Elapsed { dt_ns: 0 });
    }
    assert_eq!(core.state(), PlayState::Failed, "not stuck in Seeking");
    assert!(
        core.drain_events()
            .iter()
            .any(|e| matches!(e, VideoEvent::Failed { .. })),
        "the failure is an event"
    );
}

/// Clears the keyframe flag of every block but the first, leaving the cues where they are. The flag
/// byte sits right before each block's payload.
fn clear_keyframe_flags_after_first(bytes: &mut [u8], clip: &PatternClip) {
    for k in 1..clip.frames {
        let frame = pattern_frame(clip.width, clip.height, k);
        let at = bytes
            .windows(frame.len())
            .position(|w| w == frame.as_slice())
            .expect("the payload is in the file");
        bytes[at - 1] &= 0x7F;
    }
}

fn drain_demux(bytes: Vec<u8>) -> Result<usize, MediaError> {
    let mut d = MkvDemuxer::open(Cursor::new(bytes))?;
    let mut n = 0;
    while d.next_packet()?.is_some() {
        n += 1;
        if n > 100_000 {
            break;
        }
    }
    Ok(n)
}

#[test]
fn truncations_never_panic_and_end_early() {
    let clip = PatternClip::new(W, H, 12, 25);
    let bytes = clip.to_webm();
    for cut in (0..bytes.len()).step_by(7) {
        let r = drain_demux(bytes[..cut].to_vec());
        if let Ok(n) = r {
            assert!(n <= 12);
        }
    }
    // A truncated final cluster ends playback early without an error.
    let n = drain_demux(bytes[..bytes.len() - 40].to_vec()).expect("truncated tail is not an error");
    assert!((1..=12).contains(&n));
}

#[test]
fn bit_flips_never_panic() {
    let clip = PatternClip::new(W, H, 8, 25);
    let bytes = clip.to_webm();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..400 {
        let mut b = bytes.clone();
        for _ in 0..4 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let i = (seed % b.len() as u64) as usize;
            b[i] ^= 1 << (seed % 8);
        }
        let _ = drain_demux(b.clone());
        if let Ok(s) = Session::open(Cursor::new(b), &SessionOptions::default(), None) {
            let mut s = s;
            for _ in 0..20 {
                if !matches!(s.next_picture(), regia_media::session::FeedPoll::Frame(_)) {
                    break;
                }
            }
        }
    }
}

#[test]
fn absurd_element_sizes_are_refused_without_allocating() {
    let clip = PatternClip::new(W, H, 2, 25);
    let mut bytes = clip.to_webm();
    // Find the Tracks element (0x1654AE6B) and give it an absurd 8-byte size.
    let at = bytes
        .windows(4)
        .position(|w| w == [0x16, 0x54, 0xAE, 0x6B])
        .expect("tracks");
    bytes[at + 4..at + 12].copy_from_slice(&[0x01, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]);
    let err = probe(Cursor::new(bytes)).expect_err("refused");
    assert_eq!(err.code(), "container_invalid");
}

#[test]
fn many_tiny_clusters_and_zero_length_tracks() {
    use regia_media::mux::{MuxOptions, MuxPacket, MuxTrack, mux};
    let track = MuxTrack {
        number: 1,
        kind: TrackKind::Video,
        codec_id: RAW_VIDEO.into(),
        default_duration_ns: Some(1_000_000),
        language: "und".into(),
        default: true,
        video: Some((2, 2, Some("I420".into()))),
        audio: None,
    };
    let packets: Vec<MuxPacket> = (0..10_000)
        .map(|i| MuxPacket {
            track: 1,
            pts_ns: i * 1_000_000,
            keyframe: true,
            data: vec![0; 6],
        })
        .collect();
    let bytes = mux(&MuxOptions::default(), std::slice::from_ref(&track), &packets);
    assert_eq!(drain_demux(bytes).expect("demuxes"), 10_000);
    // A track with no packets at all.
    let empty = mux(&MuxOptions::default(), &[track], &[]);
    let (s, info) = session(empty);
    let mut core = PlayerCore::new();
    core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, true, 0.0);
    assert_eq!(core.state(), PlayState::Failed);
    assert_eq!(core.last_error().map(|e| e.code()), Some("decode_failed"));
}

#[test]
fn thirty_bad_packets_fail_the_session() {
    use regia_media::mux::{MuxOptions, MuxPacket, MuxTrack, mux};
    let track = MuxTrack {
        number: 1,
        kind: TrackKind::Video,
        codec_id: RAW_VIDEO.into(),
        default_duration_ns: Some(10_000_000),
        language: "und".into(),
        default: true,
        video: Some((2, 2, Some("I420".into()))),
        audio: None,
    };
    // Frame 0 good, then 40 packets of the wrong size (the raw decoder rejects them).
    let mut packets = vec![MuxPacket {
        track: 1,
        pts_ns: 0,
        keyframe: true,
        data: vec![0; 6],
    }];
    packets.extend((1..41).map(|i| MuxPacket {
        track: 1,
        pts_ns: i * 10_000_000,
        keyframe: true,
        data: vec![0; 5],
    }));
    let (s, info) = session(mux(&MuxOptions::default(), &[track], &packets));
    let mut core = PlayerCore::new();
    core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, true, 0.0);
    for _ in 0..50 {
        core.tick(ClockInput::Step { dt_ns: 10_000_000 });
    }
    assert_eq!(core.state(), PlayState::Failed);
    assert_eq!(core.last_error().map(|e| e.code()), Some("decode_failed"));
    assert!(core.stats().frames_corrupt >= 29);
}

#[test]
fn picture_over_the_caps_is_limit_exceeded() {
    let clip = PatternClip::new(64, 64, 2, 25);
    let mut opts = SessionOptions::default();
    opts.caps.max_width = 32;
    let err = Session::open(Cursor::new(clip.to_webm()), &opts, None).expect_err("over the cap");
    assert_eq!(err.code(), "limit_exceeded");
}

// ---- the worker path ----------------------------------------------------------------------------

#[test]
fn worker_feed_delivers_the_same_pictures_and_seeks() {
    let clip = PatternClip::new(W, H, 15, 25);
    let (s, _) = session(clip.to_webm());
    let mut worker = WorkerFeed::spawn(s, 4).expect("thread");
    let mut got = Vec::new();
    while let regia_media::session::FeedPoll::Frame(p) = worker.poll(true) {
        got.push(pattern_index(&p.planes[0]));
    }
    assert_eq!(got, (0..15).collect::<Vec<_>>());
    worker.seek(clip.frame_pts_ns(10) as u64);
    match worker.poll(true) {
        regia_media::session::FeedPoll::Frame(p) => assert_eq!(pattern_index(&p.planes[0]), 10),
        other => panic!("expected a frame, got {other:?}"),
    }
}

#[test]
fn chapters_fire_markers() {
    use regia_media::container::Marker;
    use regia_media::mux::{MuxOptions, MuxPacket, MuxTrack, mux};
    let track = MuxTrack {
        number: 1,
        kind: TrackKind::Video,
        codec_id: RAW_VIDEO.into(),
        default_duration_ns: Some(10_000_000),
        language: "und".into(),
        default: true,
        video: Some((2, 2, Some("I420".into()))),
        audio: None,
    };
    let packets: Vec<MuxPacket> = (0..10)
        .map(|i| MuxPacket {
            track: 1,
            pts_ns: i * 10_000_000,
            keyframe: true,
            data: vec![0; 6],
        })
        .collect();
    let opts = MuxOptions {
        markers: vec![Marker {
            name: "door".into(),
            time_ns: 45_000_000,
        }],
        duration_ns: Some(100_000_000),
        ..MuxOptions::default()
    };
    let (s, info) = session(mux(&opts, &[track], &packets));
    assert_eq!(info.markers.len(), 1);
    let mut core = PlayerCore::new();
    core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, true, 0.0);
    for _ in 0..8 {
        core.tick(ClockInput::Step { dt_ns: 10_000_000 });
    }
    assert!(
        core.drain_events()
            .contains(&VideoEvent::Marker { name: "door".into() })
    );
}

#[test]
fn phase_offset_starts_at_the_offset() {
    let clip = PatternClip::new(W, H, 20, 25);
    let (s, info) = session(clip.to_webm());
    let mut core = PlayerCore::new();
    core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, false, 0.4);
    assert_eq!(core.state(), PlayState::Ready);
    assert_eq!(shown(&core), 10, "0.4 s at 25 fps");
    // Game clock: the seek finishes during Opening on the first tick.
    let (s, info) = session(clip.to_webm());
    let mut core = PlayerCore::new();
    core.open(Box::new(s), info, ClockMode::Game, LoopMode::Off, false, 0.4);
    core.tick(ClockInput::Elapsed { dt_ns: 0 });
    assert_eq!(core.state(), PlayState::Ready);
    assert_eq!(shown(&core), 10);
}

/// A session whose guard reports decoding as too slow.
struct TooSlow(Session);

impl FrameFeed for TooSlow {
    fn poll(&mut self, block: bool) -> FeedPoll {
        self.0.poll(block)
    }

    fn seek(&mut self, to_ns: u64) {
        self.0.seek(to_ns);
    }

    fn stats(&self) -> FeedStats {
        let mut stats = self.0.stats();
        stats.video.decode_too_slow = true;
        stats
    }
}

#[test]
fn a_too_slow_clip_warns_once_and_stays_flagged() {
    let clip = PatternClip::new(W, H, 6, 30);
    let (s, info) = session(clip.to_webm());
    let mut core = PlayerCore::new();
    core.open(
        Box::new(TooSlow(s)),
        info,
        ClockMode::Lockstep,
        LoopMode::Off,
        true,
        0.0,
    );
    assert!(core.stats().decode_too_slow);
    assert!(core.take_slow_warning());
    for _ in 0..5 {
        core.tick(ClockInput::Step { dt_ns: 16_666_667 });
    }
    assert!(!core.take_slow_warning(), "one warning per session");
    assert!(core.stats().decode_too_slow);
}

/// A session whose end of stream can be held back as "nothing ready yet" (a starved feed).
struct HoldEnd(Session, std::sync::Arc<std::sync::atomic::AtomicBool>);

impl FrameFeed for HoldEnd {
    fn poll(&mut self, block: bool) -> FeedPoll {
        match self.0.poll(block) {
            FeedPoll::End if self.1.load(std::sync::atomic::Ordering::SeqCst) => FeedPoll::Pending,
            other => other,
        }
    }

    fn seek(&mut self, to_ns: u64) {
        self.0.seek(to_ns);
    }

    fn stats(&self) -> FeedStats {
        self.0.stats()
    }
}

#[test]
fn a_starved_tick_that_crossed_the_loop_boundary_is_rewound_with_its_loop() {
    use std::sync::{Arc, atomic::AtomicBool};
    let clip = PatternClip::new(W, H, 10, 25); // 0.4 s
    let (s, info) = session(clip.to_webm());
    let hold = Arc::new(AtomicBool::new(true));
    let mut core = PlayerCore::new();
    core.open(
        Box::new(HoldEnd(s, hold)),
        info,
        ClockMode::Game,
        LoopMode::Loop,
        true,
        0.0,
    );
    core.tick(ClockInput::Elapsed { dt_ns: 0 });
    core.tick(ClockInput::Elapsed { dt_ns: 360_000_000 });
    assert_eq!(shown(&core), 9);
    core.drain_events();
    // A 0.2 s hitch with an empty queue steps past the clip end (0.4 s) and past the starve limit.
    core.tick(ClockInput::Elapsed { dt_ns: 200_000_000 });
    assert_eq!(core.state(), PlayState::Buffering);
    let events = core.drain_events();
    assert!(events.contains(&VideoEvent::BufferingStarted));
    assert!(
        !events.iter().any(|e| matches!(e, VideoEvent::Looped { .. })),
        "the rewound tick fired no loop: {events:?}"
    );
    assert_eq!(
        core.position_ns(),
        360_000_000,
        "position stays in the pass it rewound to"
    );
}

// ---- external clock never loops the decoder -----------------------------------------------------

#[test]
fn external_clock_with_loop_ends_instead_of_rewinding() {
    let clip = PatternClip::new(W, H, 10, 25);
    let mut core = open_core(&clip, ClockMode::External, LoopMode::Loop, true);
    let mut seen = Vec::new();
    for i in 0..25u64 {
        core.tick(ClockInput::Time {
            t_ns: i * clip.frame_ns,
        });
        if core.presented().is_some() {
            seen.push(shown(&core));
        }
        if core.state() == PlayState::Ended {
            break;
        }
    }
    // Past the clip the last picture stays; the decoder never presents a second pass.
    assert!(
        seen.windows(2).all(|w| w[0] <= w[1]),
        "no wrap back to frame 0: {seen:?}"
    );
    assert_eq!(core.state(), PlayState::Ended);
    let events = core.drain_events();
    assert!(!events.iter().any(|e| matches!(e, VideoEvent::Looped { .. })));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, VideoEvent::Ended { .. }))
            .count(),
        1
    );
}

// ---- spec-conformance lane (spec 50 audit, 2026-11-10): rows that had no test ------------------

/// § 2.2 row "Failed -> close -> Closed": Failed is terminal until reopened or closed.
#[test]
fn failed_closes_to_closed() {
    let mut core = PlayerCore::new();
    core.fail_open(MediaError::invalid("x"));
    core.drain_events();
    core.close();
    assert_eq!(core.state(), PlayState::Closed);
    assert_eq!(core.drain_events(), vec![VideoEvent::Closed]);
    assert!(core.presented().is_none());
}

/// § 2.2 / § 2.5: commands outside their states answer `bad_state`: a seek while Opening, a step
/// while Playing, a frame seek on a clip without a nominal frame rate.
#[test]
fn commands_outside_their_states_are_bad_state() {
    use regia_media::mux::{MuxOptions, MuxPacket, MuxTrack, mux};
    let clip = PatternClip::new(W, H, 10, 25);
    let mut opening = open_core(&clip, ClockMode::Game, LoopMode::Off, false);
    assert_eq!(opening.state(), PlayState::Opening);
    assert_eq!(
        opening.seek_time(0.1, SeekMode::Accurate).map_err(|e| e.code()),
        Err("bad_state")
    );
    let mut playing = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, true);
    assert_eq!(playing.state(), PlayState::Playing);
    assert_eq!(playing.step_frames(1).map_err(|e| e.code()), Err("bad_state"));
    // A variable-rate clip (no DefaultDuration): `seek { frame }` is refused, `seek { time }` works.
    let video = MuxTrack {
        number: 1,
        kind: TrackKind::Video,
        codec_id: RAW_VIDEO.to_string(),
        default_duration_ns: None,
        language: "und".to_string(),
        default: true,
        video: Some((W, H, Some("I420".to_string()))),
        audio: None,
    };
    let packets: Vec<MuxPacket> = [0i64, 30, 70, 100]
        .iter()
        .map(|ms| MuxPacket {
            track: 1,
            pts_ns: ms * 1_000_000,
            keyframe: true,
            data: vec![0x80; (W * H * 3 / 2) as usize],
        })
        .collect();
    let (s, info) = session(mux(&MuxOptions::default(), &[video], &packets));
    let mut vfr = PlayerCore::new();
    vfr.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, false, 0.0);
    assert_eq!(vfr.state(), PlayState::Ready);
    assert_eq!(
        vfr.seek_frame(1, SeekMode::Accurate).map_err(|e| e.code()),
        Err("bad_state")
    );
    assert!(vfr.seek_time(0.05, SeekMode::Accurate).is_ok());
}

/// § 2.2 row "Seeking -> previous": a seek from Playing returns to Playing, from Paused to Paused,
/// each with one `Seeked`.
#[test]
fn a_seek_returns_to_the_state_it_left() {
    let clip = PatternClip::new(W, H, 20, 25);
    let mut core = open_core(&clip, ClockMode::Lockstep, LoopMode::Off, true);
    core.tick(ClockInput::Step { dt_ns: clip.frame_ns });
    core.drain_events();
    core.seek_frame(10, SeekMode::Accurate).expect("seek");
    assert_eq!(core.state(), PlayState::Playing);
    assert_eq!(shown(&core), 10);
    let seeks = |events: Vec<VideoEvent>| {
        events
            .iter()
            .filter(|e| matches!(e, VideoEvent::Seeked { .. }))
            .count()
    };
    assert_eq!(seeks(core.drain_events()), 1);
    core.pause().expect("pause");
    core.drain_events();
    core.seek_frame(4, SeekMode::Accurate).expect("seek");
    assert_eq!(core.state(), PlayState::Paused);
    assert_eq!(shown(&core), 4);
    assert_eq!(seeks(core.drain_events()), 1);
}

/// § 2.2 row "Playing -> pause -> Paused": the clock freezes; nothing advances until `play`.
#[test]
fn pause_freezes_the_clock() {
    let clip = PatternClip::new(W, H, 20, 25);
    let mut core = open_core(&clip, ClockMode::Game, LoopMode::Off, true);
    core.tick(ClockInput::Elapsed { dt_ns: 0 });
    for _ in 0..3 {
        core.tick(ClockInput::Elapsed { dt_ns: clip.frame_ns });
    }
    let (at, pos) = (shown(&core), core.position_ns());
    assert_eq!(at, 3);
    core.pause().expect("pause");
    for _ in 0..5 {
        core.tick(ClockInput::Elapsed { dt_ns: clip.frame_ns });
    }
    assert_eq!(
        (shown(&core), core.position_ns()),
        (at, pos),
        "paused: no progress"
    );
    core.play().expect("resume");
    core.tick(ClockInput::Elapsed { dt_ns: clip.frame_ns });
    assert_eq!(shown(&core), at + 1, "resumes from the frozen time, no jump");
}

/// § 2.5: in real time a late frame is skipped for the newest due one and counted in
/// `frames_dropped`.
#[test]
fn real_time_skips_late_frames_and_counts_them() {
    let clip = PatternClip::new(W, H, 20, 25);
    let mut core = open_core(&clip, ClockMode::Game, LoopMode::Off, true);
    core.tick(ClockInput::Elapsed { dt_ns: 0 });
    assert_eq!(shown(&core), 0);
    core.tick(ClockInput::Elapsed {
        dt_ns: 3 * clip.frame_ns,
    });
    assert_eq!(shown(&core), 3, "the newest due frame");
    assert_eq!(core.stats().frames_dropped, 2, "frames 1 and 2 were late");
}

/// § 2.5: holds past one frame period count `frames_repeated`; a starve under `starve_ms` (100 ms)
/// does not flicker `BufferingStarted`; past it the player buffers, and `BufferingDone` returns it
/// to Playing once pictures (here the end of the stream) arrive.
#[test]
fn a_short_starve_repeats_and_a_long_one_buffers_then_resumes() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let clip = PatternClip::new(W, H, 10, 25); // 40 ms frames, last at 360 ms
    let (s, info) = session(clip.to_webm());
    let hold = Arc::new(AtomicBool::new(true));
    let mut core = PlayerCore::new();
    core.open(
        Box::new(HoldEnd(s, hold.clone())),
        info,
        ClockMode::Game,
        LoopMode::Off,
        true,
        0.0,
    );
    core.tick(ClockInput::Elapsed { dt_ns: 0 });
    core.tick(ClockInput::Elapsed { dt_ns: 360_000_000 });
    assert_eq!(shown(&core), 9);
    core.drain_events();
    // 40 ms and 80 ms of starve: held, repeated, still Playing, no event.
    core.tick(ClockInput::Elapsed { dt_ns: 40_000_000 });
    assert_eq!(core.stats().frames_repeated, 1);
    core.tick(ClockInput::Elapsed { dt_ns: 40_000_000 });
    assert_eq!(core.stats().frames_repeated, 2);
    assert_eq!(core.state(), PlayState::Playing);
    assert!(core.drain_events().is_empty(), "a short hiccup fires no event");
    // 120 ms: past the limit.
    core.tick(ClockInput::Elapsed { dt_ns: 40_000_000 });
    assert_eq!(core.state(), PlayState::Buffering);
    assert_eq!(core.drain_events(), vec![VideoEvent::BufferingStarted]);
    core.tick(ClockInput::Elapsed { dt_ns: 40_000_000 });
    assert_eq!(core.state(), PlayState::Buffering, "still starved");
    assert!(core.drain_events().is_empty());
    // The stream end arrives: buffering is done, playback goes on and ends.
    hold.store(false, Ordering::SeqCst);
    core.tick(ClockInput::Elapsed { dt_ns: 40_000_000 });
    assert_eq!(core.state(), PlayState::Playing);
    assert_eq!(core.drain_events(), vec![VideoEvent::BufferingDone]);
    core.tick(ClockInput::Elapsed { dt_ns: 40_000_000 });
    assert_eq!(core.state(), PlayState::Ended);
    assert_eq!(shown(&core), 9, "the last frame stays");
}

/// A decoder that refuses every packet.
struct AlwaysFails;

impl VideoDecoder for AlwaysFails {
    fn decode(&mut self, _packet: &Packet) -> Result<Vec<DecodedPicture>, MediaError> {
        Err(MediaError::DecodeFailed("bad".into()))
    }
    fn flush(&mut self) {}
    fn name(&self) -> &'static str {
        "always-fails"
    }
}

/// A decoder that panics (stands in for a Rust decoder bug).
struct Panics;

impl VideoDecoder for Panics {
    fn decode(&mut self, _packet: &Packet) -> Result<Vec<DecodedPicture>, MediaError> {
        panic!("decoder bug (expected by the test)")
    }
    fn flush(&mut self) {}
    fn name(&self) -> &'static str {
        "panics"
    }
}

/// § 2.4 / § 2.9: rejected packets fail the stream after 30 in a row OR 2 s of media, whichever
/// first; a caught Rust decoder panic is `decode_failed`, never a crash.
#[test]
fn two_seconds_of_corrupt_media_fail_before_thirty_packets_and_a_panic_is_decode_failed() {
    let caps = DecodeCaps::DESKTOP;
    assert_eq!(caps.max_consecutive_corrupt, 30);
    assert_eq!(caps.max_corrupt_media_ns, 2_000_000_000);
    let mut video = GuardedVideo::new(Box::new(AlwaysFails), caps);
    // Keyframes every 500 ms, each rejected: the fifth (at 2.0 s) spans 2 s with only 5 rejects.
    let mut failed_at = None;
    for k in 0..30 {
        match video.decode(&packet(k * 500_000_000, true)) {
            Ok(Guarded::Dropped) => {}
            Ok(Guarded::Ok(_)) => panic!("nothing decodes"),
            Err(e) => {
                assert_eq!(e.code(), "decode_failed");
                failed_at = Some(k);
                break;
            }
        }
    }
    assert_eq!(failed_at, Some(4), "the reject at 2.0 s ends the stream");
    let mut video = GuardedVideo::new(Box::new(Panics), caps);
    let err = video.decode(&packet(0, true)).expect_err("a panic is an error");
    assert_eq!(err.code(), "decode_failed");
}

/// § 2.3: extra video tracks and unsupported audio tracks are ignored; the clip still plays.
#[test]
fn extra_and_unsupported_tracks_are_ignored() {
    use regia_media::mux::{MuxOptions, MuxPacket, MuxTrack, mux};
    let frame_ns = 40_000_000;
    let video = |number: u64| MuxTrack {
        number,
        kind: TrackKind::Video,
        codec_id: RAW_VIDEO.to_string(),
        default_duration_ns: Some(frame_ns),
        language: "und".to_string(),
        default: number == 1,
        video: Some((W, H, Some("I420".to_string()))),
        audio: None,
    };
    let flac = MuxTrack {
        number: 3,
        kind: TrackKind::Audio,
        codec_id: "A_FLAC".to_string(),
        default_duration_ns: None,
        language: "eng".to_string(),
        default: true,
        video: None,
        audio: Some((48_000.0, 2)),
    };
    let packets: Vec<MuxPacket> = (0..3i64)
        .flat_map(|k| {
            [1u64, 2].map(|track| MuxPacket {
                track,
                pts_ns: k * frame_ns as i64,
                keyframe: true,
                data: vec![0x80; (W * H * 3 / 2) as usize],
            })
        })
        .collect();
    let bytes = mux(&MuxOptions::default(), &[video(1), video(2), flac], &packets);
    let info = probe(Cursor::new(bytes.clone())).expect("probe");
    assert_eq!(info.tracks.len(), 3);
    assert!(info.reject_reason().is_none(), "{:?}", info.reject_reason());
    let mut s = Session::open(Cursor::new(bytes), &SessionOptions::default(), None).expect("opens");
    assert!(!s.has_audio(), "the FLAC track is ignored");
    let mut n = 0;
    while let FeedPoll::Frame(_) = s.next_picture() {
        n += 1;
    }
    assert_eq!(n, 3, "only the first video track's pictures");
}

// ---- v0.5.1 media runtime fixes (deep-zai-media findings 1 and 4) -------------------------------

/// A raw 16x8 clip of `frames` pictures at `frame_ns`, written with `opts` (the header under test).
fn raw_clip_with(frames: u32, frame_ns: u64, opts: &regia_media::mux::MuxOptions) -> Vec<u8> {
    use regia_media::mux::{MuxPacket, MuxTrack, mux};
    let track = MuxTrack {
        number: 1,
        kind: TrackKind::Video,
        codec_id: RAW_VIDEO.to_string(),
        default_duration_ns: Some(frame_ns),
        language: "und".to_string(),
        default: true,
        video: Some((W, H, Some("I420".to_string()))),
        audio: None,
    };
    let scale = opts.timestamp_scale.max(1);
    let packets: Vec<MuxPacket> = (0..frames)
        .map(|k| MuxPacket {
            track: 1,
            // Whole timestamp units, as the muxer writes them.
            pts_ns: (u64::from(k) * frame_ns / scale * scale) as i64,
            keyframe: k.is_multiple_of(5),
            data: pattern_frame(W, H, k),
        })
        .collect();
    mux(opts, std::slice::from_ref(&track), &packets)
}

fn looped_counts(events: &[VideoEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|e| match e {
            VideoEvent::Looped { count } => Some(*count),
            _ => None,
        })
        .collect()
}

/// Finding 1 (header): `TimestampScale = 1` with `Duration = 1000` declares a one-microsecond clip.
/// No video clip is shorter than a millisecond; the header is refused as `container_invalid` at
/// probe (and so at open) instead of reaching the player's loop arithmetic.
#[test]
fn a_declared_duration_under_a_millisecond_is_container_invalid() {
    use regia_media::mux::MuxOptions;
    let opts = MuxOptions {
        timestamp_scale: 1,
        duration_ns: Some(1000),
        ..MuxOptions::default()
    };
    let bytes = raw_clip_with(1, 40_000_000, &opts);
    let err = probe(Cursor::new(bytes.clone())).map(|i| i.duration_ns);
    assert_eq!(
        err.as_ref().map_err(MediaError::code),
        Err("container_invalid"),
        "probe accepted the 1000 ns duration: {err:?}"
    );
    let open = Session::open(Cursor::new(bytes), &SessionOptions::default(), None).map(|_| ());
    assert_eq!(open.map_err(|e| e.code()), Err("container_invalid"));
    // A zero duration still means "not declared", and one millisecond is the shortest one kept.
    for (units, expected) in [(0u64, Some(0u64)), (1_000_000, Some(1_000_000))] {
        let opts = MuxOptions {
            timestamp_scale: 1,
            duration_ns: Some(units),
            ..MuxOptions::default()
        };
        let info = probe(Cursor::new(raw_clip_with(1, 40_000_000, &opts))).expect("probe");
        assert_eq!(info.duration_ns, expected);
    }
}

/// Finding 1 (open): a declared duration shorter than the video track's own frame period cannot
/// be the clip's length. The player ignores it and learns the length from the stream's end (the
/// rule for files that declare none), so a looping clip plays every picture and wraps once per
/// pass instead of once per tick.
#[test]
fn a_declared_duration_shorter_than_one_frame_is_ignored_at_open() {
    use regia_media::mux::MuxOptions;
    let frame_ns = 40_000_000;
    let opts = MuxOptions {
        duration_ns: Some(1_000_000),
        ..MuxOptions::default()
    };
    let (s, info) = session(raw_clip_with(10, frame_ns, &opts));
    assert_eq!(info.duration_ns, Some(1_000_000), "the header declares 1 ms");
    let mut core = PlayerCore::new();
    core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Loop, true, 0.0);
    assert_eq!(
        core.duration(),
        None,
        "the 1 ms duration is not the clip's length"
    );
    let mut seen = Vec::new();
    for _ in 0..25 {
        core.tick(ClockInput::Step { dt_ns: frame_ns });
        seen.push(shown(&core));
    }
    let expected: Vec<u32> = (0..10).chain(0..10).chain(0..5).collect();
    assert_eq!(seen, expected);
    assert_eq!(looped_counts(&core.drain_events()), vec![1, 2]);
}

/// Finding 1 (clock): the wraps a tick crosses are counted arithmetically and reported by at most
/// `MAX_LOOPED_EVENTS_PER_TICK` events, the last carrying the total. Before the fix a one-microsecond
/// clip pushed one `Looped` per microsecond of the tick (a million events for a one-second tick).
#[test]
fn a_tick_that_crosses_many_wraps_reports_a_bounded_number_of_events() {
    let clip = PatternClip::new(W, H, 10, 25);
    for (duration_ns, frame_period, dt_ns, wraps) in [
        // A duration of one frame period (kept at open), a ten-second tick: 250 wraps.
        (40_000_000u64, Some(40_000_000u64), 10_000_000_000u64, 250u32),
        // The audit's header with no frame period to compare against: a microsecond clip, a one-second tick.
        (1000, None, 1_000_000_000, 1_000_000),
    ] {
        let (s, mut info) = session(clip.to_webm());
        info.duration_ns = Some(duration_ns);
        info.tracks[0].default_duration_ns = frame_period;
        let mut core = PlayerCore::new();
        core.open(Box::new(s), info, ClockMode::Game, LoopMode::Loop, true, 0.0);
        core.tick(ClockInput::Elapsed { dt_ns: 0 }); // opening
        assert_eq!(core.state(), PlayState::Playing);
        core.drain_events();
        core.tick(ClockInput::Elapsed { dt_ns });
        let events = core.drain_events();
        let loops = looped_counts(&events);
        assert_eq!(core.loops_done(), wraps, "every wrap is counted");
        assert!(
            // `regia_media::player::MAX_LOOPED_EVENTS_PER_TICK` (a literal, so this compiles on the
            // code before the fix and fails there).
            !loops.is_empty() && loops.len() <= 4,
            "{} Looped events for {wraps} wraps in one tick",
            loops.len()
        );
        assert_eq!(loops.last(), Some(&wraps), "the last event carries the total");
        assert!(loops.windows(2).all(|w| w[0] < w[1]), "counts rise: {loops:?}");
    }
}

/// Finding 1 also bounds `Count(n)`: the arithmetic stops at the last pass, as the loop did.
#[test]
fn a_counted_loop_stops_counting_at_its_last_pass_in_one_long_tick() {
    let clip = PatternClip::new(W, H, 10, 25);
    let mut core = open_core(&clip, ClockMode::Game, LoopMode::Count(3), true);
    core.tick(ClockInput::Elapsed { dt_ns: 0 });
    core.drain_events();
    core.tick(ClockInput::Elapsed {
        dt_ns: 10_000_000_000,
    });
    assert_eq!(core.loops_done(), 2, "three passes wrap twice");
    assert_eq!(looped_counts(&core.drain_events()), vec![1, 2]);
}

/// Finding 4: Matroska makes a track's Audio element optional (defaults: one channel, 8000 Hz).
/// A float PCM track without one decoded into no sink: `audio_format` answered `None`, so the
/// player registered no audio feed and the clip played silent with no warning.
#[test]
fn an_audio_track_without_an_audio_element_uses_the_matroska_defaults() {
    use regia_media::codec::PCM_FLOAT;
    use regia_media::mux::{MuxOptions, MuxPacket, MuxTrack, mux};
    let frame_ns = 40_000_000u64;
    let video = MuxTrack {
        number: 1,
        kind: TrackKind::Video,
        codec_id: RAW_VIDEO.to_string(),
        default_duration_ns: Some(frame_ns),
        language: "und".to_string(),
        default: true,
        video: Some((W, H, Some("I420".to_string()))),
        audio: None,
    };
    let pcm = MuxTrack {
        number: 2,
        kind: TrackKind::Audio,
        codec_id: PCM_FLOAT.to_string(),
        default_duration_ns: None,
        language: "eng".to_string(),
        default: true,
        video: None,
        audio: None,
    };
    // 40 ms of mono audio at 8000 Hz: 320 samples per picture.
    let packets: Vec<MuxPacket> = (0..3u32)
        .flat_map(|k| {
            let pts = i64::from(k) * frame_ns as i64;
            [
                MuxPacket {
                    track: 1,
                    pts_ns: pts,
                    keyframe: true,
                    data: pattern_frame(W, H, k),
                },
                MuxPacket {
                    track: 2,
                    pts_ns: pts,
                    keyframe: true,
                    data: [0.25f32.to_le_bytes(); 320].concat(),
                },
            ]
        })
        .collect();
    let bytes = mux(&MuxOptions::default(), &[video, pcm], &packets);
    let info = probe(Cursor::new(bytes.clone())).expect("probe");
    assert!(info.tracks[1].audio.is_none(), "the track has no Audio element");
    let s = Session::open(
        Cursor::new(bytes),
        &SessionOptions::default(),
        Some(Box::new(CollectSink::default())),
    )
    .expect("opens");
    assert!(s.has_audio(), "the PCM track is selected");
    assert_eq!(
        s.audio_format(),
        Some((1, 8000)),
        "the decoding track reports the Matroska defaults, so a feed is registered"
    );
}
