// SPDX-License-Identifier: MPL-2.0
//! Compressed codecs (ADR-1869, specs/50 milestones 2-3) against committed real-encoder fixtures.
//!
//! The fixtures in `tests/fixtures/` were encoded once with an offline encoder (commands in
//! `tests/fixtures/README.md`). Expected picture hashes are FNV-1a 64 over the Y, U and V planes
//! (tightly packed, 16-bit little-endian samples above 8 bits) as decoded by two independent AV1
//! decoders, which agreed byte for byte; AV1 reconstruction is normative, so any conforming decoder must
//! match them exactly. Audio expectations are sample values and 20 ms block RMS from the reference Opus
//! and Vorbis decoders, compared with a tolerance (float decoders may differ in the last bits, specs/50 § 7).
//! No wall clock anywhere: fixed steps and counters.
#![cfg(any(feature = "av1", feature = "opus", feature = "vorbis"))]

use std::io::Cursor;

#[cfg(feature = "av1")]
use regia_media::codec::ChromaFormat;
use regia_media::codec::{self, CodecVerdict};
use regia_media::container::{Demuxer, MkvDemuxer, Packet, TrackKind};
// Every session test needs the video track, so AV1 (the Opus one too).
#[cfg(feature = "av1")]
use regia_media::session::{AudioChoice, Session, SessionOptions};

#[cfg(any(feature = "av1", feature = "opus"))]
const AV1_OPUS: &[u8] = include_bytes!("fixtures/av1_opus_64x48.webm");
#[cfg(feature = "av1")]
const AV1_10BIT: &[u8] = include_bytes!("fixtures/av1_10bit_65x33.webm");
#[cfg(any(feature = "av1", feature = "vorbis"))]
const AV1_VORBIS: &[u8] = include_bytes!("fixtures/av1_vorbis_32x32.webm");

#[cfg(feature = "av1")]
/// 12 frames, 64x48, 8-bit 4:2:0, 24 fps, keyframes at 0 and 6, hidden alt-ref frames and
/// show-existing-frame packets (3 bytes) in between.
const HASH_A: [u64; 12] = [
    0xafc9_53cb_9d9f_59c6,
    0x307e_0459_b4e2_3edb,
    0x14d6_2b0b_68a4_facc,
    0x913a_f608_e55c_0745,
    0x50dc_f50e_897f_2d6c,
    0xfafc_ab6e_243a_4d94,
    0x53f7_2c2f_ea86_47a9,
    0x6844_765c_41f2_37ea,
    0xc2b3_894a_4cda_c674,
    0x110d_4ead_39b4_fe93,
    0x6877_4307_bec8_1c95,
    0x82cf_3eff_f497_c095,
];
#[cfg(feature = "av1")]
/// 4 frames, 65x33 (odd sizes), 10-bit 4:2:0.
const HASH_B: [u64; 4] = [
    0xdc72_aa45_5bcf_6cd7,
    0xabb6_9c2e_4c4c_9ab0,
    0x70b7_5d2e_7fd5_b03d,
    0xe63f_b9ae_ca08_03cf,
];
#[cfg(feature = "av1")]
/// 6 frames, 32x32, 8-bit 4:2:0, 12 fps.
const HASH_C: [u64; 6] = [
    0x54d0_fcb6_9411_0ba6,
    0x54d0_fcb6_9411_0ba6,
    0x708f_0f0f_aca0_6ad7,
    0x708f_0f0f_aca0_6ad7,
    0x8cf6_3458_29ad_05b0,
    0x8cf6_3458_29ad_05b0,
];

#[cfg(feature = "av1")]
/// One clip of [`LAYOUTS`]: the bytes, its layout and bit depth, whether the hash covers luma only
/// (monochrome: ffmpeg writes no chroma), and the frame hashes.
type LayoutClip = (&'static [u8], ChromaFormat, u8, bool, [u64; 4]);

#[cfg(feature = "av1")]
/// The chroma layouts, bit depths and film grain the other clips do not cover: 48x32, 30 fps, 4 frames.
const LAYOUTS: [LayoutClip; 5] = [
    (
        include_bytes!("fixtures/av1_444_48x32.webm"),
        ChromaFormat::Yuv444,
        8,
        false,
        [
            0xd0f2_a8b3_3edb_0458,
            0xd0f2_a8b3_3edb_0458,
            0x4962_eebd_a6df_5e0b,
            0x4962_eebd_a6df_5e0b,
        ],
    ),
    (
        include_bytes!("fixtures/av1_422_10bit_48x32.webm"),
        ChromaFormat::Yuv422,
        10,
        false,
        [
            0x07db_acfb_ee52_dfb0,
            0x07db_acfb_ee52_dfb0,
            0x46d8_db90_1da6_da22,
            0x46d8_db90_1da6_da22,
        ],
    ),
    (
        include_bytes!("fixtures/av1_12bit_48x32.webm"),
        ChromaFormat::Yuv420,
        12,
        false,
        [
            0xbea4_d083_61db_60f1,
            0xbea4_d083_61db_60f1,
            0xbea4_d083_61db_60f1,
            0xbea4_d083_61db_60f1,
        ],
    ),
    (
        AV1_GRAY,
        ChromaFormat::Yuv420,
        8,
        true,
        [
            0xd4e6_7adc_e2a5_4b80,
            0xd4e6_7adc_e2a5_4b80,
            0x4ee0_4be9_a806_f3d9,
            0x4ee0_4be9_a806_f3d9,
        ],
    ),
    (
        include_bytes!("fixtures/av1_grain_48x32.webm"),
        ChromaFormat::Yuv420,
        8,
        false,
        [
            0xa966_0a95_6b7a_13ec,
            0x6364_0f73_4bc4_4590,
            0x72f2_1c30_9120_cb78,
            0x0c1d_70ad_5cd6_24e6,
        ],
    ),
];
#[cfg(feature = "av1")]
const AV1_GRAY: &[u8] = include_bytes!("fixtures/av1_gray_48x32.webm");

#[cfg(feature = "av1")]
fn fnv(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(feature = "av1")]
fn options(audio: AudioChoice) -> SessionOptions {
    SessionOptions {
        audio,
        ..SessionOptions::default()
    }
}

fn track_packets(bytes: &[u8], kind: TrackKind) -> (regia_media::TrackInfo, Vec<Packet>) {
    let mut demux = MkvDemuxer::open(Cursor::new(bytes.to_vec())).expect("fixture demuxes");
    let track = demux
        .info()
        .tracks
        .iter()
        .find(|t| t.kind == kind)
        .expect("track present")
        .clone();
    let mut packets = Vec::new();
    while let Some(p) = demux.next_packet().expect("clean fixture") {
        if p.track == track.number {
            packets.push(p);
        }
    }
    (track, packets)
}

#[cfg(any(feature = "opus", feature = "vorbis"))]
fn rms(s: &[f32]) -> f64 {
    (s.iter().map(|v| f64::from(*v) * f64::from(*v)).sum::<f64>() / s.len().max(1) as f64).sqrt()
}

#[cfg(any(feature = "opus", feature = "vorbis"))]
/// Goertzel power of `freq` relative to the signal power (1.0 for a pure tone at `freq`).
fn tone_share(s: &[f32], rate: f64, freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / rate;
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in s {
        let s0 = f64::from(x) + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
    let n = s.len() as f64;
    // A sine of amplitude a gives |X|^2 = (a n / 2)^2 and a mean square of a^2 / 2.
    (2.0 * power / (n * n)) / rms(s).powi(2).max(1e-12)
}

#[cfg(feature = "av1")]
mod av1 {
    use super::*;
    use regia_media::codec::DecodedPicture;
    use regia_media::container::probe;
    use regia_media::session::{FeedPoll, FrameFeed};

    fn picture_hash(p: &DecodedPicture) -> u64 {
        fnv(p.planes.iter().flat_map(|plane| plane.iter().copied()))
    }

    fn decode_all(bytes: &[u8]) -> Vec<DecodedPicture> {
        let mut s = Session::open(Cursor::new(bytes.to_vec()), &options(AudioChoice::None), None)
            .expect("AV1 session opens");
        assert_eq!(s.decoder_name(), "rav1d");
        let mut out = Vec::new();
        loop {
            match s.next_picture() {
                FeedPoll::Frame(p) => out.push(p),
                FeedPoll::End => return out,
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// ADR-2862, the sequencer's seek on real AV1 (keyframes at 0 and 6, hidden alt-ref frames between):
    /// a visit from any side shows the picture the reference decoders hashed for that frame.
    #[test]
    fn show_at_lands_on_the_reference_decoders_picture_from_any_side() {
        use regia_media::player::{ClockInput, ClockMode, LoopMode, PlayerCore};

        let session = Session::open(Cursor::new(AV1_OPUS.to_vec()), &options(AudioChoice::None), None)
            .expect("AV1 session opens");
        let info = session.info().clone();
        let mut core = PlayerCore::new();
        core.open(
            Box::new(session),
            info,
            ClockMode::External,
            LoopMode::Off,
            false,
            0.0,
        );
        core.tick(ClockInput::Time { t_ns: 0 });
        // Frame k's nominal time, plus half a millisecond of timestamp rounding.
        let at = |k: u64| k * 1_000_000_000 / 24 + 500_000;
        let visit = |core: &mut PlayerCore, k: usize| {
            assert!(core.show_at(at(k as u64), true, u32::MAX), "frame {k}");
            picture_hash(core.presented().expect("a picture"))
        };
        for (k, want) in HASH_A.iter().enumerate() {
            assert_eq!(visit(&mut core, k), *want, "forward to frame {k}");
        }
        for (k, want) in HASH_A.iter().enumerate().rev() {
            assert_eq!(visit(&mut core, k), *want, "backward to frame {k}");
        }
        for i in 0..48usize {
            let k = (i * 7) % 12;
            assert_eq!(visit(&mut core, k), HASH_A[k], "scattered visit {i} to frame {k}");
        }
    }

    #[test]
    fn av1_is_supported_with_the_feature() {
        assert_eq!(codec::classify("V_AV1"), CodecVerdict::Supported);
        let info = probe(Cursor::new(AV1_OPUS.to_vec())).expect("probe");
        assert!(info.reject_reason().is_none());
        let v = info.video_track().expect("video");
        assert_eq!(v.codec_id, "V_AV1");
        let params = v.video.as_ref().expect("video params");
        assert_eq!((params.width, params.height), (64, 48));
    }

    #[test]
    fn av1_8bit_matches_the_reference_decoders() {
        let pics = decode_all(AV1_OPUS);
        let hashes: Vec<u64> = pics.iter().map(picture_hash).collect();
        assert_eq!(hashes, HASH_A);
        for (k, p) in pics.iter().enumerate() {
            assert_eq!((p.width, p.height, p.bit_depth), (64, 48, 8));
            assert_eq!(p.planes[1].len(), 32 * 24);
            // 24 fps at a 1 ms timestamp scale: 0, 42, 83, 125, ... ms.
            let want_ms = (k as i64 * 1000 + 12) / 24;
            assert_eq!(p.pts_ns / 1_000_000, want_ms, "frame {k}");
        }
    }

    #[test]
    fn av1_10bit_odd_size_matches_the_reference_decoders() {
        let pics = decode_all(AV1_10BIT);
        assert_eq!(pics.iter().map(picture_hash).collect::<Vec<_>>(), HASH_B);
        let p = &pics[0];
        assert_eq!(
            (p.width, p.height, p.bit_depth, p.sample_bytes()),
            (65, 33, 10, 2)
        );
        assert_eq!(p.planes[0].len(), 65 * 33 * 2);
        assert_eq!(p.planes[1].len(), 33 * 17 * 2);
        assert!(
            (0..65).all(|x| p.sample(0, x, 10) < 1024),
            "10-bit samples stay in range"
        );
    }

    #[test]
    fn every_layout_and_bit_depth_matches_the_reference_decoders() {
        for (bytes, chroma, bit_depth, luma_only, hashes) in LAYOUTS {
            let pics = decode_all(bytes);
            let got: Vec<u64> = pics
                .iter()
                .map(|p| {
                    if luma_only {
                        fnv(p.planes[0].iter().copied())
                    } else {
                        picture_hash(p)
                    }
                })
                .collect();
            assert_eq!(got, hashes, "{chroma:?} {bit_depth}-bit");
            for p in &pics {
                assert_eq!((p.width, p.height), (48, 32));
                assert_eq!((p.chroma, p.bit_depth), (chroma, bit_depth));
            }
        }
    }

    #[test]
    fn monochrome_gets_neutral_chroma() {
        let pics = decode_all(AV1_GRAY);
        for p in &pics {
            assert_eq!(p.planes[1].len(), 24 * 16);
            assert!(p.planes[1].iter().chain(p.planes[2].iter()).all(|v| *v == 128));
        }
    }

    #[test]
    fn av1_with_vorbis_audio_track_decodes_video() {
        let pics = decode_all(AV1_VORBIS);
        assert_eq!(pics.iter().map(picture_hash).collect::<Vec<_>>(), HASH_C);
    }

    #[test]
    fn repeat_decodes_are_bit_identical() {
        let a: Vec<u64> = decode_all(AV1_OPUS).iter().map(picture_hash).collect();
        let b: Vec<u64> = decode_all(AV1_OPUS).iter().map(picture_hash).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn session_seek_restarts_at_a_cue_and_continues_exactly() {
        let mut s = Session::open(Cursor::new(AV1_OPUS.to_vec()), &options(AudioChoice::None), None)
            .expect("session");
        for _ in 0..3 {
            assert!(matches!(s.next_picture(), FeedPoll::Frame(_)));
        }
        // Frame 9 is at 375 ms; the demuxer resumes at the cue at or before it. The encoder wrote one
        // cluster with one cue, so that is frame 0; either way decoding restarts at a keyframe (0 or 6)
        // and every picture after it is exact.
        s.seek(375_000_000);
        let mut after = Vec::new();
        while let FeedPoll::Frame(p) = s.next_picture() {
            after.push(picture_hash(&p));
        }
        let start = HASH_A.len() - after.len();
        assert!(start == 0 || start == 6, "restarted at frame {start}");
        assert_eq!(after, HASH_A[start..]);
        // Back to the start: the decoder is flushed and resent the av1C configuration.
        s.seek(0);
        match s.next_picture() {
            FeedPoll::Frame(p) => assert_eq!(picture_hash(&p), HASH_A[0]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn accurate_seek_presents_the_exact_frame() {
        use regia_media::player::{ClockMode, LoopMode, PlayerCore, SeekMode};
        let open = || {
            let s = Session::open(Cursor::new(AV1_OPUS.to_vec()), &options(AudioChoice::None), None)
                .expect("session");
            let info = s.info().clone();
            let mut core = PlayerCore::new();
            core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, false, 0.0);
            core
        };
        let mut core = open();
        // 375 ms is frame 9's timestamp: an accurate seek shows frame 9, decoded through the
        // hidden and show-existing frames between the keyframe and it.
        core.seek_time(0.375, SeekMode::Accurate).expect("seek");
        assert_eq!(core.presented().map(picture_hash), Some(HASH_A[9]));
        // Accurate lands on the first picture at or after the target: 100 ms -> frame 3 (125 ms).
        core.seek_time(0.1, SeekMode::Accurate).expect("seek back");
        assert_eq!(core.presented().map(picture_hash), Some(HASH_A[3]));
        let mut key = open();
        key.seek_time(0.375, SeekMode::Keyframe).expect("keyframe seek");
        let h = key.presented().map(picture_hash).expect("presented");
        assert!(h == HASH_A[0] || h == HASH_A[6], "a keyframe is shown");
    }

    #[test]
    fn lockstep_player_presents_reference_frames_in_order() {
        use regia_media::player::{ClockInput, ClockMode, LoopMode, PlayState, PlayerCore};
        let run = || {
            let s = Session::open(Cursor::new(AV1_OPUS.to_vec()), &options(AudioChoice::None), None)
                .expect("session");
            let info = s.info().clone();
            let mut core = PlayerCore::new();
            core.open(Box::new(s), info, ClockMode::Lockstep, LoopMode::Off, true, 0.0);
            let mut shown = Vec::new();
            for _ in 0..40 {
                core.tick(ClockInput::Step { dt_ns: 16_666_667 });
                if let Some(p) = core.presented() {
                    let h = picture_hash(p);
                    shown.push(HASH_A.iter().position(|x| *x == h).expect("a reference frame"));
                }
            }
            (shown, core.state(), core.stats().frames_dropped)
        };
        let (shown, state, dropped) = run();
        assert_eq!(state, PlayState::Ended);
        assert_eq!(dropped, 0);
        assert!(shown.windows(2).all(|w| w[0] <= w[1]), "{shown:?}");
        for k in 0..12 {
            assert!(shown.contains(&k), "frame {k} was presented: {shown:?}");
        }
        assert_eq!(run().0, shown, "repeat runs are identical");
    }

    #[test]
    fn corrupt_av1_packets_are_dropped_and_decoding_resumes_at_the_keyframe() {
        let (track, packets) = track_packets(AV1_OPUS, TrackKind::Video);
        let mut dec = regia_media::isolate::GuardedVideo::new(
            codec::video_decoder_for(&track).expect("decoder"),
            regia_media::isolate::DecodeCaps::default(),
        );
        let mut hashes = Vec::new();
        for (k, p) in packets.iter().enumerate() {
            let mut p = p.clone();
            if k == 1 {
                // Garbage inside the first group of pictures: an OBU header with a forbidden bit.
                p.data = vec![0xff; p.data.len()];
            }
            if let regia_media::isolate::Guarded::Ok(pics) = dec.decode(&p).expect("not fatal") {
                hashes.extend(pics.iter().map(picture_hash));
            }
        }
        assert!(dec.stats().corrupt >= 1);
        // Everything from the second keyframe on is exact again.
        assert!(hashes.ends_with(&HASH_A[6..]), "{hashes:x?}");
    }

    #[test]
    fn caps_refuse_oversized_av1_before_output() {
        let (track, packets) = track_packets(AV1_OPUS, TrackKind::Video);
        let caps = regia_media::isolate::DecodeCaps {
            max_width: 32,
            max_height: 32,
            ..Default::default()
        };
        let mut dec = regia_media::isolate::GuardedVideo::new(
            codec::video_decoder_with(&track, &caps).expect("decoder"),
            caps,
        );
        let mut shown = 0;
        for p in packets.iter().take(6) {
            match dec.decode(p) {
                Ok(regia_media::isolate::Guarded::Ok(pics)) => shown += pics.len(),
                Ok(_) => {}
                Err(e) => {
                    assert_eq!(e.code(), "decode_failed");
                    break;
                }
            }
        }
        assert_eq!(shown, 0, "no picture over the caps leaves the guard");
    }
}

#[cfg(feature = "opus")]
mod opus {
    use super::*;

    /// First samples and 20 ms block RMS of the reference decoder (pre-skip already removed).
    const FIRST: [f32; 3] = [0.027_161, 0.046_875, 0.065_521];
    const AT_1000: [f32; 2] = [-0.083_069, -0.070_007];
    const BLOCK_RMS: [f64; 6] = [0.082_223, 0.088_508, 0.088_527, 0.088_440, 0.088_332, 0.088_522];

    fn decode(packets: &[Packet], track: &regia_media::TrackInfo) -> Vec<f32> {
        let mut dec = codec::audio_decoder_for(track).expect("Opus decoder");
        assert_eq!(dec.output_format(), Some((1, 48_000)));
        let mut out = Vec::new();
        for p in packets {
            dec.decode(p, &mut out).expect("clean packet");
        }
        out
    }

    #[test]
    fn opus_matches_the_reference_decoder() {
        assert_eq!(codec::classify("A_OPUS"), CodecVerdict::Supported);
        let (track, packets) = track_packets(AV1_OPUS, TrackKind::Audio);
        assert_eq!(track.codec_id, "A_OPUS");
        let out = decode(&packets, &track);
        // 0.5 s plus the encoder's end padding (the demuxer does not trim it).
        assert!(
            out.len() >= 24_000 && out.len() < 24_000 + 960 * 2,
            "{}",
            out.len()
        );
        for (i, want) in FIRST.iter().enumerate() {
            assert!((out[i] - want).abs() < 2e-3, "sample {i}: {} vs {want}", out[i]);
        }
        for (i, want) in AT_1000.iter().enumerate() {
            assert!((out[1000 + i] - want).abs() < 2e-3, "sample {}", 1000 + i);
        }
        for (b, want) in BLOCK_RMS.iter().enumerate() {
            let got = rms(&out[b * 960..(b + 1) * 960]);
            assert!((got - want).abs() < 1e-3, "block {b}: {got} vs {want}");
        }
        assert!(tone_share(&out[960..24_000], 48_000.0, 1000.0) > 0.95);
    }

    #[test]
    fn opus_flush_restarts_pre_skip_only_at_the_start() {
        let (track, packets) = track_packets(AV1_OPUS, TrackKind::Audio);
        let straight = decode(&packets, &track);
        let mut dec = codec::audio_decoder_for(&track).expect("decoder");
        let mut out = Vec::new();
        for p in &packets[..5] {
            dec.decode(p, &mut out).expect("packet");
        }
        dec.flush();
        out.clear();
        for p in &packets {
            dec.decode(p, &mut out).expect("packet");
        }
        assert_eq!(
            out, straight,
            "a flush and replay from the start equals a fresh decode"
        );
        dec.flush();
        out.clear();
        dec.decode(&packets[10], &mut out).expect("packet");
        assert_eq!(out.len(), 960, "no pre-skip in the middle of the stream");
        let mut empty = packets[11].clone();
        empty.data.clear();
        assert!(
            dec.decode(&empty, &mut out).is_err(),
            "an empty packet is refused, not concealed"
        );
    }

    // The session needs the video track to open, so this one also needs AV1.
    #[cfg(feature = "av1")]
    #[test]
    fn opus_session_feeds_48k_mono_audio() {
        use regia_media::session::{CollectSink, FeedPoll};
        let sink = CollectSink::default();
        let mut s = Session::open(
            Cursor::new(AV1_OPUS.to_vec()),
            &options(AudioChoice::Default),
            Some(Box::new(sink)),
        )
        .expect("session");
        assert!(s.has_audio());
        assert_eq!(s.audio_format(), Some((1, 48_000)));
        let mut frames = 0;
        while let FeedPoll::Frame(_) = s.next_picture() {
            frames += 1;
        }
        assert_eq!(frames, 12);
        let stats = regia_media::session::FrameFeed::stats(&s);
        assert!(stats.audio.packets >= 20, "{stats:?}");
        assert_eq!(stats.audio.corrupt, 0);
    }

    #[test]
    fn corrupt_opus_packets_become_silence_not_panics() {
        let (track, packets) = track_packets(AV1_OPUS, TrackKind::Audio);
        let mut dec = regia_media::isolate::GuardedAudio::new(
            codec::audio_decoder_for(&track).expect("decoder"),
            regia_media::isolate::DecodeCaps::default(),
        );
        let mut out = Vec::new();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for p in &packets {
            let mut p = p.clone();
            for b in p.data.iter_mut() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                if seed.is_multiple_of(5) {
                    *b = (seed >> 24) as u8;
                }
            }
            let before = out.len();
            dec.decode(&p, &mut out, 960).expect("never fatal");
            assert!(out.len() > before || p.data.is_empty());
        }
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
    }
}

#[cfg(feature = "vorbis")]
mod vorbis {
    use super::*;

    /// Reference decoder: interleaved stereo (440 Hz left, 880 Hz right), 44.1 kHz.
    const FIRST: [f32; 4] = [-0.000_244, -0.001_221, 0.007_538, 0.014_221];
    const AT_2000: [f32; 2] = [-0.018_066, -0.033_966];
    const AT_30000: [f32; 2] = [-0.105_347, 0.114_532];
    const TOTAL: usize = 44_160;

    #[test]
    fn vorbis_matches_the_reference_decoder() {
        assert_eq!(codec::classify("A_VORBIS"), CodecVerdict::Supported);
        let (track, packets) = track_packets(AV1_VORBIS, TrackKind::Audio);
        assert_eq!(track.codec_id, "A_VORBIS");
        let mut dec = codec::audio_decoder_for(&track).expect("Vorbis decoder");
        assert_eq!(dec.output_format(), Some((2, 44_100)));
        let mut out = Vec::new();
        for p in &packets {
            dec.decode(p, &mut out).expect("clean packet");
        }
        assert_eq!(out.len(), TOTAL);
        for (at, want) in [(0, &FIRST[..]), (2000, &AT_2000[..]), (30_000, &AT_30000[..])] {
            for (i, w) in want.iter().enumerate() {
                assert!(
                    (out[at + i] - w).abs() < 1e-3,
                    "sample {}: {} vs {w}",
                    at + i,
                    out[at + i]
                );
            }
        }
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        let right: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
        assert!(tone_share(&left[4410..], 44_100.0, 440.0) > 0.95);
        assert!(tone_share(&right[4410..], 44_100.0, 880.0) > 0.95);
    }

    #[test]
    fn bad_vorbis_headers_are_refused_at_open() {
        let (mut track, _) = track_packets(AV1_VORBIS, TrackKind::Audio);
        track.codec_private.truncate(40);
        let Err(err) = codec::audio_decoder_for(&track) else {
            panic!("truncated headers must be refused");
        };
        assert_eq!(err.code(), "container_invalid");
        track.codec_private.clear();
        assert!(codec::audio_decoder_for(&track).is_err());
    }

    #[test]
    fn a_vorbis_ident_past_eight_channels_is_refused() {
        let (mut track, _) = track_packets(AV1_VORBIS, TrackKind::Audio);
        let ident = track
            .codec_private
            .windows(7)
            .position(|w| w == b"\x01vorbis")
            .expect("the identification header");
        // Packet type, magic, then a four-byte version: the channel count is byte 11.
        track.codec_private[ident + 11] = 9;
        let Err(err) = codec::audio_decoder_for(&track) else {
            panic!("nine channels must be refused");
        };
        assert_eq!(err.code(), "container_invalid");
        assert!(err.to_string().contains("more channels"), "{err}");
    }

    #[test]
    fn corrupt_vorbis_packets_become_silence_not_panics() {
        let (track, packets) = track_packets(AV1_VORBIS, TrackKind::Audio);
        let mut dec = regia_media::isolate::GuardedAudio::new(
            codec::audio_decoder_for(&track).expect("decoder"),
            regia_media::isolate::DecodeCaps::default(),
        );
        let mut out = Vec::new();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for p in &packets {
            let mut p = p.clone();
            for b in p.data.iter_mut() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                if seed.is_multiple_of(7) {
                    *b = (seed >> 32) as u8;
                }
            }
            dec.decode(&p, &mut out, 2 * 1024).expect("never fatal");
        }
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
    }
}
