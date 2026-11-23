// SPDX-License-Identifier: MPL-2.0
//! Opus voice coding through the session's codec seam (`docs/specs/59-voice-chat.md` § 8 acceptance
//! 3, 5, 14; ADR-1593 § 4). Opus output depends on the library build, so these assert signal
//! properties and sizes, never bytes (spec § 7).
#![cfg(feature = "voice-opus")]

use regia_app::App;
use regia_audio::voice_chat::*;
use regia_media::voice_opus::{OpusVoiceCodec, VoiceOpusPlugin, check_packet, has_lbrr, opus_codecs};

const PROFILES: [VoiceQuality; 3] = [VoiceQuality::Narrow, VoiceQuality::Standard, VoiceQuality::High];

fn sine(rate: u32, seconds: f32, hz: f64, amp: f32) -> Vec<f32> {
    let n = (rate as f32 * seconds) as usize;
    (0..n)
        .map(|i| (std::f64::consts::TAU * hz * i as f64 / f64::from(rate)).sin() as f32 * amp)
        .collect()
}

/// A voiced-speech stand-in: a 140 Hz pulse train's harmonics under a 4 Hz syllable envelope.
fn speech(rate: u32, seconds: f32) -> Vec<f32> {
    let n = (rate as f32 * seconds) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            let envelope = 0.55 + 0.45 * (std::f64::consts::TAU * 4.0 * t).sin();
            let voice: f64 = (1..=12)
                .map(|h| {
                    let h = f64::from(h);
                    (std::f64::consts::TAU * 140.0 * h * t).sin() / h
                })
                .sum();
            (voice * envelope * 0.12) as f32
        })
        .collect()
}

fn rms_db(samples: &[f32]) -> f32 {
    let mean = samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len().max(1) as f64;
    (10.0 * mean.max(1e-12).log10()) as f32
}

/// Goertzel power of `samples` at `hz`.
fn power_at(samples: &[f32], rate: u32, hz: f64) -> f64 {
    let w = std::f64::consts::TAU * hz / f64::from(rate);
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for x in samples {
        let s0 = f64::from(*x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

/// The strongest frequency on a 10 Hz grid.
fn peak_hz(samples: &[f32], rate: u32) -> f64 {
    (5..=200)
        .map(|k| f64::from(k) * 10.0)
        .max_by(|a, b| power_at(samples, rate, *a).total_cmp(&power_at(samples, rate, *b)))
        .unwrap_or(0.0)
}

fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        ab += f64::from(*x) * f64::from(*y);
        aa += f64::from(*x) * f64::from(*x);
        bb += f64::from(*y) * f64::from(*y);
    }
    (ab / (aa * bb).sqrt().max(1e-12)) as f32
}

fn encode_all(codec: &mut OpusVoiceCodec, input: &[f32], frame: usize) -> Vec<Vec<u8>> {
    input
        .chunks_exact(frame)
        .map(|f| {
            let mut out = vec![0u8; MAX_PAYLOAD];
            let n = codec.encode(f, &mut out).expect("encodes");
            out.truncate(n);
            out
        })
        .collect()
}

fn decode_all(codec: &mut OpusVoiceCodec, packets: &[Vec<u8>], frame: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frame * packets.len()];
    for (p, o) in packets.iter().zip(out.chunks_exact_mut(frame)) {
        assert_eq!(codec.decode(Some(p), false, o).expect("decodes"), frame);
    }
    out
}

#[test]
fn a_tone_round_trips_at_every_profile() {
    for quality in PROFILES {
        let rate = quality.sample_rate();
        let frame = quality.frame_samples();
        let input = sine(rate, 1.0, 440.0, 0.25);
        let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
        let mut rx = OpusVoiceCodec::new(quality, 5).expect("codec");
        let packets = encode_all(&mut tx, &input, frame);
        assert_eq!(packets.len(), 50);
        // Constrained VBR: three times the target frame is the most a voiced onset takes.
        let cap = (quality.target_bps() / 8 / 50) as usize * 3;
        let biggest = packets.iter().map(Vec::len).max().unwrap_or(0);
        assert!(biggest <= cap, "{quality:?}: {biggest} > {cap}");
        let output = decode_all(&mut rx, &packets, frame);
        // The first 100 ms holds the codec delay.
        let settled = &output[frame * 5..];
        let delta = rms_db(settled) - rms_db(&input[frame * 5..]);
        assert!(delta.abs() < 2.0, "{quality:?}: {delta} dB");
        let peak = peak_hz(settled, rate);
        assert!((peak - 440.0).abs() <= 10.0, "{quality:?}: {peak} Hz");
    }
}

#[test]
fn every_complexity_builds_and_more_is_refused() {
    for complexity in 0..=10 {
        OpusVoiceCodec::new(VoiceQuality::Narrow, complexity).expect("codec");
    }
    assert!(matches!(
        OpusVoiceCodec::new(VoiceQuality::Narrow, 11),
        Err(VoiceChatError::InvalidParams(_))
    ));
}

#[test]
fn wire_payloads_match_the_bandwidth_table() {
    // Spec § 2.11, re-measured: the payload at target per profile, within 10 % over 10 s of speech.
    for (quality, table) in [
        (VoiceQuality::Narrow, 30.0),
        (VoiceQuality::Standard, 50.0),
        (VoiceQuality::High, 80.0),
    ] {
        let rate = quality.sample_rate();
        let frame = quality.frame_samples();
        let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
        let packets = encode_all(&mut tx, &speech(rate, 10.0), frame);
        let total: usize = packets.iter().map(Vec::len).sum();
        let mean = total as f64 / packets.len() as f64;
        assert!(
            (mean - table).abs() <= table * 0.1,
            "{quality:?}: {mean} B vs {table} B"
        );
        assert!(packets.iter().all(|p| p.len() <= MAX_PAYLOAD));
    }
}

#[test]
fn in_band_fec_recovers_a_lost_frame_where_the_rate_allows_it() {
    // libopus keeps LBRR for bit rates that can afford it: not the narrow profile.
    for (quality, expect_lbrr) in [
        (VoiceQuality::Narrow, false),
        (VoiceQuality::Standard, true),
        (VoiceQuality::High, true),
    ] {
        let rate = quality.sample_rate();
        let frame = quality.frame_samples();
        let input = speech(rate, 2.0);
        let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
        tx.set_expected_loss(20);
        let packets = encode_all(&mut tx, &input, frame);
        let carrying = packets.iter().filter(|p| has_lbrr(p)).count();
        assert_eq!(
            carrying > packets.len() / 2,
            expect_lbrr,
            "{quality:?}: {carrying}"
        );
        let lost = (40..packets.len() - 1)
            .find(|i| has_lbrr(&packets[i + 1]) == expect_lbrr)
            .expect("a frame to lose");
        let run = |use_fec: bool| {
            let mut rx = OpusVoiceCodec::new(quality, 5).expect("codec");
            let mut out = vec![0.0f32; frame * packets.len()];
            for (i, o) in out.chunks_exact_mut(frame).enumerate() {
                let n = if i == lost {
                    if use_fec {
                        rx.decode(Some(&packets[i + 1]), true, o)
                    } else {
                        rx.decode(None, false, o)
                    }
                } else {
                    rx.decode(Some(&packets[i]), false, o)
                };
                assert_eq!(n.expect("decodes"), frame);
            }
            out
        };
        // Compare against the same stream with nothing lost: the codec delay and its noise cancel.
        let clean = decode_all(
            &mut OpusVoiceCodec::new(quality, 5).expect("codec"),
            &packets,
            frame,
        );
        let window = frame * lost..frame * (lost + 1);
        let recovered = run(true);
        let concealed = run(false);
        let with_fec = correlation(&recovered[window.clone()], &clean[window.clone()]);
        let plain = correlation(&concealed[window.clone()], &clean[window.clone()]);
        assert!(with_fec > 0.7, "{quality:?}: fec {with_fec}");
        assert!(plain > 0.5, "{quality:?}: concealment {plain}");
        // The frames after the loss stay in range, at speech level.
        let after = frame * (lost + 1)..frame * (lost + 4);
        for out in [&recovered, &concealed] {
            assert!(
                (rms_db(&out[after.clone()]) - rms_db(&clean[after.clone()])).abs() < 6.0,
                "{quality:?}"
            );
        }
    }
}

#[test]
fn concealment_stays_bounded_and_fades() {
    let quality = VoiceQuality::Standard;
    let frame = quality.frame_samples();
    let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let mut rx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let packets = encode_all(&mut tx, &speech(24_000, 1.0), frame);
    let _ = decode_all(&mut rx, &packets, frame);
    let mut levels = Vec::new();
    let mut out = vec![0.0f32; frame];
    for _ in 0..50 {
        rx.decode(None, false, &mut out).expect("conceals");
        assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.5));
        levels.push(rms_db(&out));
    }
    assert!(levels[49] < levels[0] - 20.0, "{levels:?}");
    // The first concealed frames carry the voice on, at its level, not a blast and not a hole.
    let speech_level = rms_db(&speech(24_000, 1.0));
    assert!((levels[0] - speech_level).abs() < 9.0, "{levels:?}");
    assert!(levels.windows(2).all(|w| w[1] <= w[0]), "{levels:?}");
}

#[test]
fn packets_the_library_would_conceal_are_refused_or_silent() {
    let quality = VoiceQuality::Standard;
    let frame = quality.frame_samples();
    let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let mut rx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let packets = encode_all(&mut tx, &speech(24_000, 1.0), frame);
    let _ = decode_all(&mut rx, &packets, frame);
    let mut out = vec![0.5f32; frame];
    // One and two bytes: libopus would run its concealment; here they are a silent frame.
    for short in [&[0x08u8][..], &[0x08, 0x00]] {
        rx.decode(Some(short), false, &mut out).expect("silent");
        assert!(out.iter().all(|s| *s == 0.0));
    }
    // Empty frames of a multi-frame packet, a CELT-only packet after SILK (its transition conceals
    // the SILK tail), stereo, and the wrong duration: refused before the library.
    for hostile in [
        vec![0x09u8, 0, 0, 0],
        vec![0x0Au8, 0, 0, 0],
        vec![0x0Bu8, 0x82, 0, 0, 0],
        vec![0xFCu8, 1, 2, 3, 4, 5],
        vec![0x98u8, 1, 2, 3, 4, 5],
        vec![0x0Cu8, 1, 2, 3, 4, 5],
        vec![0x00u8, 1, 2, 3, 4, 5],
        vec![0xC8u8, 1, 2, 3, 4, 5],
    ] {
        assert!(check_packet(&hostile).is_err(), "{hostile:?}");
        assert!(rx.decode(Some(&hostile), false, &mut out).is_err(), "{hostile:?}");
    }
    assert!(check_packet(&[]).is_err());
}

#[test]
fn the_encoder_only_makes_packets_the_gate_accepts() {
    let noise: Vec<f32> = {
        let mut rng = Pcg32::new(5);
        (0..48_000).map(|_| (rng.next_f32() - 0.5) * 1.8).collect()
    };
    for quality in PROFILES {
        let rate = quality.sample_rate();
        let frame = quality.frame_samples();
        for input in [
            speech(rate, 2.0),
            sine(rate, 1.0, 1000.0, 0.9),
            noise.clone(),
            vec![0.0; frame * 50],
            vec![0.002; frame * 50],
        ] {
            let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
            for p in encode_all(&mut tx, &input, frame) {
                if p.len() > 2 {
                    check_packet(&p).expect("the gate takes our own packets");
                }
            }
        }
    }
}

#[test]
fn losses_on_a_pure_tone_do_not_ring_above_the_talker() {
    // The worst case for a decoder with a gap in its memory: nothing but prediction.
    let quality = VoiceQuality::Standard;
    let frame = quality.frame_samples();
    let input = sine(24_000, 4.0, 440.0, 0.25);
    let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let mut rx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let packets = encode_all(&mut tx, &input, frame);
    let mut out = vec![0.0f32; frame];
    let mut heard = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        if i > 25 && i % 9 == 0 {
            rx.decode(None, false, &mut out).expect("conceals");
        } else {
            rx.decode(Some(p), false, &mut out).expect("decodes");
        }
        heard.push(rms_db(&out));
    }
    let talker = rms_db(&input);
    let loudest = heard[25..].iter().copied().fold(f32::MIN, f32::max);
    assert!(loudest < talker + 7.0, "{loudest} over {talker}");
}

#[test]
fn a_talker_who_gets_louder_after_a_loss_comes_through() {
    let quality = VoiceQuality::Narrow;
    let frame = quality.frame_samples();
    let mut input = speech(16_000, 1.0);
    for s in &mut input {
        *s *= 0.2;
    }
    input.extend(speech(16_000, 4.0));
    let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let mut rx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let packets = encode_all(&mut tx, &input, frame);
    let mut out = vec![0.0f32; frame * packets.len()];
    for (i, (p, o)) in packets.iter().zip(out.chunks_exact_mut(frame)).enumerate() {
        if i == 48 {
            rx.decode(None, false, o).expect("conceals");
        } else {
            rx.decode(Some(p), false, o).expect("decodes");
        }
    }
    let tail = &out[out.len() - frame * 50..];
    let original = &input[input.len() - frame * 50..];
    assert!(
        (rms_db(tail) - rms_db(original)).abs() < 3.0,
        "{} vs {}",
        rms_db(tail),
        rms_db(original)
    );
}

#[test]
fn a_long_loss_never_leaves_the_decoder_loud() {
    // The library's concealment would pin the output at full scale from here on.
    let quality = VoiceQuality::Narrow;
    let frame = quality.frame_samples();
    let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let mut rx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let packets = encode_all(&mut tx, &speech(16_000, 2.0), frame);
    let mut out = vec![0.0f32; frame];
    for (i, p) in packets.iter().enumerate() {
        if i % 7 == 3 {
            rx.decode(None, false, &mut out).expect("conceals");
        } else {
            rx.decode(Some(p), false, &mut out).expect("decodes");
        }
        assert!(rms_db(&out) < -6.0, "frame {i}: {}", rms_db(&out));
    }
}

#[test]
fn hostile_payloads_never_panic_and_never_write_past_a_frame() {
    let quality = VoiceQuality::Narrow;
    let frame = quality.frame_samples();
    let mut rng = Pcg32::new(99);
    let mut rx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let mut errors = 0;
    let mut decoded = 0;
    for round in 0..4000 {
        let len = (rng.next_up_to(399) + 1) as usize;
        let mut p: Vec<u8> = (0..len).map(|_| rng.next_up_to(255) as u8).collect();
        match round % 3 {
            // Long frames and code-3 packets: the TOC of a 120 ms multi-frame packet.
            0 => p[0] = 0b1111_1011,
            // A shape the gate lets through, so the bytes reach the library.
            1 => p[0] = 0b0000_1000,
            _ => {}
        }
        let mut out = vec![0.0f32; frame + 4];
        out[frame..].fill(7.0);
        match rx.decode(Some(&p), round % 2 == 1, &mut out) {
            Ok(n) => {
                assert_eq!(n, frame);
                decoded += 1;
            }
            Err(_) => errors += 1,
        }
        assert!(out[..frame].iter().all(|s| s.is_finite()));
        assert_eq!(&out[frame..], &[7.0; 4], "nothing past the frame");
    }
    assert!(errors > 0, "some of that must be refused");
    assert!(decoded > 1000, "and the rest reaches the decoder: {decoded}");
    let mut out = vec![0.0f32; frame];
    assert!(rx.decode(Some(&[0u8; MAX_PAYLOAD + 1]), false, &mut out).is_err());
    let mut short = vec![0.0f32; frame - 1];
    assert!(rx.decode(None, false, &mut short).is_err());
}

#[test]
fn bad_input_frames_are_refused_or_cleaned() {
    let quality = VoiceQuality::Narrow;
    let frame = quality.frame_samples();
    let mut tx = OpusVoiceCodec::new(quality, 5).expect("codec");
    let mut out = vec![0u8; MAX_PAYLOAD];
    assert!(tx.encode(&vec![0.0; frame - 1], &mut out).is_err());
    let dirty = vec![f32::NAN; frame];
    assert!(tx.encode(&dirty, &mut out).expect("encodes") > 0);
    let loud = vec![f32::INFINITY; frame];
    assert!(tx.encode(&loud, &mut out).expect("encodes") > 0);
    let mut tiny = [0u8; 1];
    let _ = tx.encode(&vec![0.1; frame], &mut tiny);
}

#[test]
fn comfort_decodes_as_a_silent_frame() {
    let quality = VoiceQuality::Narrow;
    let frame = quality.frame_samples();
    let mut codec = OpusVoiceCodec::new(quality, 5).expect("codec");
    let mut buf = [0u8; 8];
    let n = codec.encode_comfort(&mut buf).expect("comfort");
    let mut out = vec![1.0f32; frame];
    assert_eq!(
        codec.decode(Some(&buf[..n]), false, &mut out).expect("decodes"),
        frame
    );
    assert!(out.iter().all(|s| *s == 0.0));
    // Recovering a lost frame from a comfort packet has nothing to recover: it conceals.
    assert_eq!(
        codec.decode(Some(&buf[..n]), true, &mut out).expect("conceals"),
        frame
    );
    assert!(out.iter().all(|s| s.is_finite()));
}

#[test]
fn reset_forgets_the_stream() {
    let quality = VoiceQuality::Narrow;
    let frame = quality.frame_samples();
    let mut codec = OpusVoiceCodec::new(quality, 5).expect("codec");
    let packets = encode_all(&mut codec, &speech(16_000, 0.4), frame);
    let _ = decode_all(&mut codec, &packets, frame);
    codec.reset();
    let mut out = vec![0.0f32; frame];
    codec.decode(None, false, &mut out).expect("conceals");
    assert!(rms_db(&out) < -70.0, "{}", rms_db(&out));
}

fn opus_session(quality: VoiceQuality) -> VoiceChatSession {
    let settings = VoiceChatSettings {
        deterministic: true,
        quality,
        ..VoiceChatSettings::default()
    };
    let mut session = VoiceChatSession::new(settings, VoiceChatUserSettings::default()).expect("session");
    session.set_codec_provider(opus_codecs()).expect("provider");
    session
}

/// `session`'s frames of `sender` while `frames` voice frames pass.
fn played(session: &mut VoiceChatSession, sender: u16, frames: usize, talk: bool) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..frames {
        session.tick(talk);
        for (id, _, samples) in session.playout() {
            if *id == sender {
                out.extend_from_slice(samples);
            }
        }
    }
    out
}

#[test]
fn the_full_loopback_chain_carries_a_tone_through_opus() {
    for quality in PROFILES {
        let mut session = opus_session(quality);
        session.set_mode(VoiceMode::Open);
        session
            .set_channel(1, Some(true), Some(true), None)
            .expect("channel");
        session.set_test_source(Some(TestSource::sine(440.0, -12.0)));
        session.set_loopback(true, false).expect("loopback");
        let audio = played(&mut session, LOOPBACK_SENDER, 100, false);
        assert!(session.stats().frames_encoded >= 99);
        let packets = session.take_outgoing();
        let first = VoicePacket::parse(&packets[0]).expect("parses");
        assert_eq!(first.codec, VoiceCodecId::Opus);
        assert!(first.payload.len() < 200);
        let rate = quality.sample_rate();
        let settled = &audio[audio.len() / 2..];
        assert!((peak_hz(settled, rate) - 440.0).abs() <= 10.0, "{quality:?}");
        assert!(rms_db(settled) > -30.0, "{quality:?}: {}", rms_db(settled));
    }
}

#[test]
fn loss_through_the_jitter_buffer_is_recovered_or_concealed_and_output_stays_clean() {
    for (loss, burst) in [(5.0, 1.0), (10.0, 1.0), (20.0, 2.0), (40.0, 3.0)] {
        let mut session = opus_session(VoiceQuality::Standard);
        session
            .set_channel(2, Some(true), Some(true), None)
            .expect("channel");
        session
            .simulate_network(SimulatorSettings {
                loss_pct: loss,
                burst_len: burst,
                latency_frames: 2,
                jitter_frames: 1,
                seed: 17,
                ..SimulatorSettings::default()
            })
            .expect("network");
        session.set_fake_speaker(9, Some(TestSource::sine(300.0, -14.0)));
        let audio = played(&mut session, 9, 500, false);
        assert!(audio.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "{loss}");
        let (_, stats) = session.speaker_stats()[0];
        let repaired = stats.jitter.recovered + stats.jitter.concealed + stats.jitter.silenced;
        assert!(repaired > 0, "{loss}: {stats:?}");
        assert_eq!(stats.decode_errors, 0, "{loss}");
        assert!(stats.jitter.played > 200, "{loss}: {stats:?}");
    }
}

#[test]
fn end_of_talk_closes_the_spurt_at_the_remote_speaker() {
    let mut a = opus_session(VoiceQuality::Narrow);
    let mut b = opus_session(VoiceQuality::Narrow);
    a.set_channel(1, Some(true), Some(true), None).expect("join");
    b.set_channel(1, Some(true), None, None).expect("join");
    a.set_local_sender(3);
    a.set_test_source(Some(TestSource::sine(440.0, -12.0)));
    for _ in 0..20 {
        a.tick(true);
        for p in a.take_outgoing() {
            b.receive_packet(&p);
        }
        b.tick(false);
    }
    assert_eq!(b.speaker(3).expect("speaker").state().name(), "speaking");
    // Self mute mid-speech sends the comfort packet.
    a.set_self_muted(true);
    for p in a.take_outgoing() {
        assert_eq!(VoicePacket::parse(&p).expect("parses").payload.len(), 0);
        b.receive_packet(&p);
    }
    for _ in 0..20 {
        b.tick(false);
    }
    assert_eq!(b.speaker(3).expect("speaker").state().name(), "silent");
    assert_eq!(b.speaker(3).expect("speaker").stats().decode_errors, 0);
}

#[test]
fn a_pcm_peer_is_still_decoded() {
    let mut session = opus_session(VoiceQuality::Narrow);
    session.set_channel(1, Some(true), None, None).expect("join");
    let mut pcm = PcmCodec::new(VoiceQuality::Narrow);
    let frame = vec![0.25f32; 320];
    let mut payload = vec![0u8; 700];
    for seq in 0..10u16 {
        let n = pcm.encode(&frame, &mut payload).expect("encodes");
        let packet = VoicePacket {
            flags: VoiceFlags::default(),
            sender: 5,
            channel: 1,
            seq,
            timestamp: u32::from(seq) * FRAME_TIMESTAMP_STEP,
            quality: VoiceQuality::Narrow,
            codec: VoiceCodecId::Pcm,
            payload: &payload[..n],
        };
        let mut wire = Vec::new();
        packet.write(&mut wire);
        session.receive_packet(&wire);
        session.tick(false);
    }
    assert_eq!(session.speaker(5).expect("speaker").stats().unsupported, 0);
    assert_eq!(session.speaker(5).expect("speaker").state().name(), "speaking");
}

#[test]
fn the_plugin_works_in_either_order() {
    let build = |opus_first: bool| {
        let mut app = App::new();
        if opus_first {
            app.add_plugins(VoiceOpusPlugin);
            app.add_plugins(VoiceChatPlugin::default());
        } else {
            app.add_plugins(VoiceChatPlugin::default());
            app.add_plugins(VoiceOpusPlugin);
        }
        let session = app.world().resource::<VoiceChatResource>();
        let codec = session.0.lock().expect("lock").status().codec;
        assert_eq!(codec, "opus", "opus first: {opus_first}");
    };
    build(true);
    build(false);
}

/// Two frames, a packet the gate lets through with a junk body, the ctl calls, reset and drop: every
/// `unsafe` call in `voice_opus` once. Miri stops inside the library on this path
/// (`docs/06-testing-and-ci.md` § 14), so this one runs natively.
fn crosses_the_library_boundary(quality: VoiceQuality) {
    let rate = quality.sample_rate();
    let frame = quality.frame_samples();
    let mut tx = OpusVoiceCodec::new(quality, 0).expect("codec");
    let mut rx = OpusVoiceCodec::new(quality, 0).expect("codec");
    tx.set_target_bitrate(quality.target_bps());
    tx.set_expected_loss(10);
    let input = speech(rate, 3.0 * frame as f32 / rate as f32);
    let packets = encode_all(&mut tx, &input[..frame * 2], frame);
    assert_eq!(packets.len(), 2);
    let output = decode_all(&mut rx, &packets, frame);
    assert!(output.iter().all(|s| s.is_finite()));

    let mut junk = vec![0x5Au8; 40];
    junk[0] = 0b0000_1000;
    let mut guarded = vec![7.0f32; frame + 4];
    if rx.decode(Some(&junk), false, &mut guarded).is_ok() {
        assert!(guarded[..frame].iter().all(|s| s.is_finite()));
    }
    assert_eq!(&guarded[frame..], &[7.0; 4], "nothing past the frame");

    tx.reset();
    rx.reset();
}

#[test]
fn the_library_boundary_narrow() {
    crosses_the_library_boundary(VoiceQuality::Narrow);
}

#[test]
fn the_library_boundary_high() {
    crosses_the_library_boundary(VoiceQuality::High);
}

/// The decode path alone, with no encoded frame: the part of the library Miri can reach
/// (`docs/06-testing-and-ci.md` § 14).
#[test]
fn the_decoder_boundary_takes_a_junk_packet() {
    let quality = VoiceQuality::Narrow;
    let frame = quality.frame_samples();
    let mut rx = OpusVoiceCodec::new(quality, 0).expect("codec");
    let mut junk = vec![0x5Au8; 40];
    junk[0] = 0b0000_1000;
    let mut guarded = vec![7.0f32; frame + 4];
    if rx.decode(Some(&junk), false, &mut guarded).is_ok() {
        assert!(guarded[..frame].iter().all(|s| s.is_finite()));
    }
    assert_eq!(&guarded[frame..], &[7.0; 4], "nothing past the frame");
    rx.reset();
}
