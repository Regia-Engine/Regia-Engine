// SPDX-License-Identifier: MPL-2.0
//! Public-API tests of the voice chat feature (`docs/specs/59-voice-chat.md` § 8). No device,
//! no wall clock: frame counters and seeded inputs only.

use regia_audio::voice_chat::*;

/// Energy of `samples` at `freq` (Goertzel), normalized per sample.
fn tone_power(samples: &[f32], freq: f64, rate: u32) -> f64 {
    let w = std::f64::consts::TAU * freq / f64::from(rate);
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for x in samples {
        let s0 = f64::from(*x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2) / (samples.len() as f64).powi(2)
}

fn rms(samples: &[f32]) -> f64 {
    (samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / samples.len() as f64).sqrt()
}

fn pcm_packet(seq: u16, sender: u16, channel: u16, end: bool, quality: VoiceQuality, level: f32) -> Vec<u8> {
    let mut codec = PcmCodec::new(quality);
    let frame = vec![level; quality.frame_samples()];
    let mut payload = vec![0u8; 1 + 2 * frame.len()];
    let n = codec.encode(&frame, &mut payload).expect("encodes");
    let mut wire = Vec::new();
    VoicePacket {
        flags: VoiceFlags::default().with(VoiceFlags::END_OF_TALK, end),
        sender,
        channel,
        seq,
        timestamp: u32::from(seq).wrapping_mul(FRAME_TIMESTAMP_STEP),
        quality,
        codec: VoiceCodecId::Pcm,
        payload: &payload[..n],
    }
    .write(&mut wire);
    wire
}

// ----- 1. packet --------------------------------------------------------------------------

#[test]
fn packet_header_round_trips_at_the_extremes() {
    for (seq, ts) in [(0u16, 0u32), (65_535, u32::MAX), (1, 4_294_967_000)] {
        for bits in 0..16u8 {
            let packet = VoicePacket {
                flags: VoiceFlags::from_bits(bits << 4),
                sender: 0xFFFF,
                channel: 0xFFFE,
                seq,
                timestamp: ts,
                quality: VoiceQuality::High,
                codec: VoiceCodecId::Opus,
                payload: &[1, 2, 3],
            };
            let mut wire = Vec::new();
            packet.write(&mut wire);
            assert_eq!(wire.len(), HEADER_LEN + 3);
            assert_eq!(VoicePacket::parse(&wire).expect("parses"), packet);
        }
    }
}

#[test]
fn packet_parse_rejects_short_unknown_and_oversized() {
    assert!(VoicePacket::parse(&[]).is_err());
    assert!(VoicePacket::parse(&[1; 11]).is_err());
    let good = pcm_packet(1, 1, 1, false, VoiceQuality::Narrow, 0.1);
    let mut v2 = good.clone();
    v2[0] = (v2[0] & 0xF0) | 2;
    assert!(VoicePacket::parse(&v2).is_err(), "newer version is refused");
    let mut v0 = good.clone();
    v0[0] &= 0xF0;
    assert!(VoicePacket::parse(&v0).is_err(), "version 0 is refused");
    let mut bad_quality = good.clone();
    bad_quality[11] = 0x0F;
    assert!(VoicePacket::parse(&bad_quality).is_err());
    let mut bad_codec = good;
    bad_codec[11] = 0xF0;
    assert!(VoicePacket::parse(&bad_codec).is_err());
    let mut opus = Vec::new();
    VoicePacket {
        flags: VoiceFlags::default(),
        sender: 1,
        channel: 1,
        seq: 0,
        timestamp: 0,
        quality: VoiceQuality::Standard,
        codec: VoiceCodecId::Opus,
        payload: &[0; MAX_PAYLOAD + 1],
    }
    .write(&mut opus);
    assert!(
        VoicePacket::parse(&opus).is_err(),
        "an Opus payload over 400 bytes is a bug"
    );
    opus.pop();
    assert!(VoicePacket::parse(&opus).is_ok());
}

#[test]
fn packet_parse_never_panics_on_random_bytes() {
    let mut rng = Pcg32::new(99);
    for _ in 0..5000 {
        let len = rng.next_up_to(64) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| rng.next_u32() as u8).collect();
        let _ = VoicePacket::parse(&bytes);
    }
}

// ----- 2. serial numbers and the jitter buffer --------------------------------------------

#[test]
fn serial_arithmetic_wraps() {
    assert_eq!(seq_diff(0, 65_535), 1);
    assert_eq!(seq_diff(65_535, 0), -1);
    assert!(seq_newer(2, 65_534));
    assert!(!seq_newer(65_534, 2));
}

#[test]
fn jitter_buffer_wraps_dedups_slots_reorders_and_drops_late() {
    let mut jb = JitterBuffer::new(JitterSettings::default());
    assert_eq!(jb.insert(65_533, 0, false, &[1], 0), InsertOutcome::Accepted);
    assert_eq!(jb.insert(65_535, 0, false, &[3], 1), InsertOutcome::Accepted);
    assert_eq!(jb.insert(65_534, 0, false, &[2], 2), InsertOutcome::Accepted);
    assert_eq!(jb.insert(0, 0, false, &[4], 3), InsertOutcome::Accepted);
    assert_eq!(jb.insert(0, 0, false, &[4], 3), InsertOutcome::Duplicate);
    assert_eq!(jb.depth_frames(), 4);
    let mut order = Vec::new();
    for _ in 0..4 {
        match jb.pull() {
            Playout::Frame(p) => order.push(p[0]),
            other => panic!("expected a frame, got {other:?}"),
        }
    }
    assert_eq!(
        order,
        [1, 2, 3, 4],
        "reordered packets play in sequence order across the wrap"
    );
    assert_eq!(jb.insert(65_534, 0, false, &[2], 9), InsertOutcome::Late);
    let s = jb.stats();
    assert_eq!((s.duplicates, s.late, s.played), (1, 1, 4));
}

#[test]
fn jitter_buffer_burst_loss_conceals_then_silences_then_resumes() {
    let mut jb = JitterBuffer::new(JitterSettings::default());
    for seq in 0..4u16 {
        jb.insert(seq, u32::from(seq) * 960, false, &[seq as u8], u64::from(seq));
    }
    // Lost: 4..=11 (eight frames). 12 and 13 arrive.
    jb.insert(12, 12 * 960, false, &[12], 12);
    jb.insert(13, 13 * 960, true, &[13], 13);
    for _ in 0..4 {
        assert!(matches!(jb.pull(), Playout::Frame(_)));
    }
    let out: Vec<Playout> = (0..8).map(|_| jb.pull()).collect();
    // Frames 4..=10 have nothing buffered after them; 11 is followed by the buffered 12 and
    // could be recovered, but by then the run is over five, so it is silent.
    assert!(out[..5].iter().all(|p| *p == Playout::Conceal), "{out:?}");
    assert!(out[5..].iter().all(|p| *p == Playout::Silence), "{out:?}");
    assert!(matches!(jb.pull(), Playout::Frame(p) if p == [12]));
    assert!(matches!(jb.pull(), Playout::Frame(p) if p == [13]));
    assert_eq!(jb.pull(), Playout::Finished);
}

#[test]
fn jitter_buffer_single_loss_recovers_from_the_next_packet() {
    let mut jb = JitterBuffer::new(JitterSettings::default());
    for seq in [0u16, 1, 3, 4] {
        jb.insert(seq, 0, false, &[seq as u8], 0);
    }
    assert!(matches!(jb.pull(), Playout::Frame(_)));
    assert!(matches!(jb.pull(), Playout::Frame(_)));
    assert!(matches!(jb.pull(), Playout::Recover(p) if p == [3]));
    assert!(matches!(jb.pull(), Playout::Frame(p) if p == [3]));
    assert_eq!(jb.stats().recovered, 1);
}

#[test]
fn jitter_buffer_stall_is_one_underrun_and_recovers_inside_the_bounds() {
    let settings = JitterSettings::default();
    let mut jb = JitterBuffer::new(settings);
    let mut seq = 0u16;
    let mut frame = 0u64;
    for _ in 0..30 {
        jb.insert(seq, u32::from(seq) * 960, false, &[1], frame);
        seq = seq.wrapping_add(1);
        jb.pull();
        frame += 1;
    }
    // 150 ms stall: nothing arrives for eight frames while the sender keeps counting.
    for _ in 0..8 {
        jb.pull();
        frame += 1;
        seq = seq.wrapping_add(1);
    }
    for _ in 0..30 {
        jb.insert(seq, u32::from(seq) * 960, false, &[1], frame);
        seq = seq.wrapping_add(1);
        jb.pull();
        frame += 1;
    }
    let s = jb.stats();
    assert_eq!(s.underruns, 1, "{s:?}");
    assert!(jb.target_ms() >= settings.min_ms && jb.target_ms() <= settings.max_ms);
    assert!(s.played >= 40, "playback resumed: {s:?}");
}

#[test]
fn jitter_depth_stays_inside_bounds_under_simulated_jitter() {
    let settings = JitterSettings::default();
    let mut jb = JitterBuffer::new(settings);
    let mut sim = VoiceChatSimulator::new(SimulatorSettings {
        latency_frames: 2,
        jitter_frames: 3,
        seed: 5,
        ..Default::default()
    })
    .expect("settings");
    let mut max_depth = 0;
    for n in 0..300u64 {
        sim.advance();
        sim.send_packet(&pcm_packet(n as u16, 1, 1, false, VoiceQuality::Narrow, 0.1));
        while let Some(p) = sim.poll_packet() {
            let pk = VoicePacket::parse(&p).expect("parses");
            jb.insert(pk.seq, pk.timestamp, false, pk.payload, n);
        }
        jb.pull();
        max_depth = max_depth.max(jb.depth_frames());
        assert!(jb.target_ms() >= settings.min_ms && jb.target_ms() <= settings.max_ms);
    }
    assert!(
        max_depth as u32 * FRAME_MS <= settings.max_ms + 3 * FRAME_MS,
        "depth {max_depth}"
    );
}

/// Spec 59 acceptance 6, the clause the depth test above does not assert: with up to 60 ms of
/// simulated jitter, underruns may happen while the first talk spurt teaches the buffer its depth,
/// and none happen in the later spurts (the target is updated at the spurt boundary).
#[test]
fn jitter_buffer_has_no_underruns_after_the_first_talk_spurt() {
    let settings = JitterSettings::default();
    let mut jb = JitterBuffer::new(settings);
    let mut sim = VoiceChatSimulator::new(SimulatorSettings {
        latency_frames: 2,
        jitter_frames: 3,
        seed: 11,
        ..Default::default()
    })
    .expect("settings");
    // Four spurts of 100 frames with 25-frame pauses; seq keeps counting through the pauses
    // (spec § 2.8: unsent frames under 1 s still consume a number).
    const SPURT: u64 = 100;
    const PAUSE: u64 = 25;
    let mut after_first = None;
    let mut seq: u16 = 0;
    for frame in 0..4 * (SPURT + PAUSE) {
        let in_spurt = frame % (SPURT + PAUSE) < SPURT;
        sim.advance();
        if in_spurt {
            let end = frame % (SPURT + PAUSE) == SPURT - 1;
            sim.send_packet(&pcm_packet(seq, 1, 1, end, VoiceQuality::Narrow, 0.1));
        }
        seq = seq.wrapping_add(1);
        while let Some(p) = sim.poll_packet() {
            let pk = VoicePacket::parse(&p).expect("parses");
            jb.insert(
                pk.seq,
                pk.timestamp,
                pk.flags.contains(VoiceFlags::END_OF_TALK),
                pk.payload,
                frame,
            );
        }
        jb.pull();
        if frame == SPURT + PAUSE - 1 {
            after_first = Some(jb.stats().underruns);
        }
        assert!(jb.target_ms() >= settings.min_ms && jb.target_ms() <= settings.max_ms);
    }
    let after_first = after_first.expect("first spurt ended");
    let stats = jb.stats();
    assert!(stats.played > 3 * SPURT, "the later spurts played: {stats:?}");
    assert_eq!(
        stats.underruns, after_first,
        "underruns after the first talk spurt: {stats:?}"
    );
}

/// Spec 59 § 2.8, byte for byte: little-endian fields, `version` in the low nibble of byte 0
/// with the flags in the high bits, profile in the low nibble of byte 11 and the codec id in its
/// high nibble, payload from byte 12. The round-trip tests above cannot catch a layout that is
/// wrong the same way in `write` and `parse`.
#[test]
fn packet_header_matches_the_spec_layout_byte_for_byte() {
    let payload = [0xAAu8, 0xBB];
    let mut wire = Vec::new();
    VoicePacket {
        flags: VoiceFlags::default()
            .with(VoiceFlags::END_OF_TALK, true)
            .with(VoiceFlags::RESET, true),
        sender: 0x0102,
        channel: 0x0304,
        seq: 0x0506,
        timestamp: 0x0708_090A,
        quality: VoiceQuality::Standard,
        codec: VoiceCodecId::Opus,
        payload: &payload,
    }
    .write(&mut wire);
    assert_eq!(
        wire,
        [
            0x31, // version 1 | end_of_talk 0x10 | reset 0x20
            0x02, 0x01, // sender
            0x04, 0x03, // channel
            0x06, 0x05, // seq
            0x0A, 0x09, 0x08, 0x07, // timestamp
            0x11, // quality Standard (1) | codec Opus (1) << 4
            0xAA, 0xBB, // payload
        ]
    );
    let mut multi = wire.clone();
    multi[0] = 0x81; // version 1 | multi; priority stays clear
    multi[11] = 0x02; // High, PCM
    let parsed = VoicePacket::parse(&multi).expect("parses");
    assert!(parsed.flags.contains(VoiceFlags::MULTI));
    assert!(!parsed.flags.contains(VoiceFlags::END_OF_TALK));
    assert_eq!(
        (parsed.quality, parsed.codec),
        (VoiceQuality::High, VoiceCodecId::Pcm)
    );
}

// ----- 3. codec ---------------------------------------------------------------------------

#[test]
fn pcm_codec_round_trips_a_sine_on_every_profile() {
    for quality in [VoiceQuality::Narrow, VoiceQuality::Standard, VoiceQuality::High] {
        let rate = quality.sample_rate();
        let mut src = TestSource::sine(440.0, -6.0);
        let mut enc = PcmCodec::new(quality);
        let mut dec = PcmCodec::new(quality);
        let mut sent = Vec::new();
        let mut got = Vec::new();
        for _ in 0..50 {
            let mut frame = vec![0.0f32; quality.frame_samples()];
            src.fill(&mut frame, rate);
            let mut bytes = vec![0u8; 1 + 2 * frame.len()];
            let n = enc.encode(&frame, &mut bytes).expect("encodes");
            assert_eq!(n, 1 + 2 * frame.len());
            let mut out = vec![0.0f32; frame.len()];
            assert_eq!(
                dec.decode(Some(&bytes[..n]), false, &mut out).expect("decodes"),
                frame.len()
            );
            sent.extend_from_slice(&frame);
            got.extend_from_slice(&out);
        }
        for (a, b) in sent.iter().zip(&got) {
            assert!((a - b).abs() <= 1.0 / 32_768.0 + 1e-6, "quantization only");
        }
        let db = 20.0 * (rms(&got) / rms(&sent)).log10();
        assert!(db.abs() < 0.01, "{quality:?}: {db} dB");
        assert!(
            tone_power(&got, 440.0, rate) > 20.0 * tone_power(&got, 550.0, rate),
            "{quality:?} peaks at 440 Hz"
        );
    }
}

#[test]
fn pcm_codec_rejects_bad_sizes_and_conceals_with_a_fade() {
    let q = VoiceQuality::Narrow;
    let mut codec = PcmCodec::new(q);
    let mut out = vec![0.0f32; q.frame_samples()];
    assert!(codec.encode(&[0.0; 10], &mut [0; 1000]).is_err());
    assert!(codec.encode(&vec![0.0; q.frame_samples()], &mut [0; 10]).is_err());
    assert!(codec.decode(Some(&[0, 1, 2]), false, &mut out).is_err());
    assert!(codec.decode(Some(&[9]), false, &mut out).is_err());
    assert!(codec.decode(Some(&[]), false, &mut out).is_err());
    // A comfort frame is its mode byte alone (ADR-1809).
    let mut comfort = [0u8; 8];
    let n = codec.encode_comfort(&mut comfort).expect("comfort");
    codec
        .decode(Some(&comfort[..n]), false, &mut out)
        .expect("silence");
    assert!(
        codec.decode(Some(&comfort), false, &mut out).is_err(),
        "trailing bytes"
    );
    let mut bytes = vec![0u8; 1 + 2 * q.frame_samples()];
    codec
        .encode(&vec![0.5; q.frame_samples()], &mut bytes)
        .expect("encodes");
    codec.decode(Some(&bytes), false, &mut out).expect("decodes");
    codec.decode(None, false, &mut out).expect("conceals");
    let first = out[0];
    codec.decode(None, false, &mut out).expect("conceals");
    assert!(
        first < 0.5 && out[0] < first && out[0] > 0.0,
        "fades per concealed frame"
    );
    let mut one = [0u8; 4];
    assert_eq!(codec.encode_comfort(&mut one).expect("comfort"), 1);
    codec.decode(Some(&one[..1]), false, &mut out).expect("decodes");
    assert!(out.iter().all(|s| *s == 0.0));
    let nan = vec![f32::NAN; q.frame_samples()];
    codec.encode(&nan, &mut bytes).expect("encodes");
    codec.decode(Some(&bytes), false, &mut out).expect("decodes");
    assert!(out.iter().all(|s| *s == 0.0));
}

#[test]
fn new_codec_reports_opus_as_unsupported() {
    assert!(new_codec(VoiceCodecId::Pcm, VoiceQuality::High, 5).is_ok());
    assert_eq!(
        new_codec(VoiceCodecId::Opus, VoiceQuality::High, 5)
            .err()
            .map(|e| e.code()),
        Some("unsupported")
    );
    assert_eq!(
        new_codec(VoiceCodecId::Pcm, VoiceQuality::High, 11)
            .err()
            .map(|e| e.code()),
        Some("invalid_params")
    );
}

// ----- 4. capture conversion --------------------------------------------------------------

#[test]
fn resampler_is_chunk_invariant_and_keeps_level() {
    let n = 44_100usize;
    let input: Vec<f32> = (0..n)
        .map(|i| (std::f64::consts::TAU * 1000.0 * i as f64 / 44_100.0).sin() as f32 * 0.5)
        .collect();
    let mut whole = Resampler::new(44_100, 24_000).expect("rates");
    let mut a = Vec::new();
    whole.process(&input, &mut a);
    let mut chunked = Resampler::new(44_100, 24_000).expect("rates");
    let mut b = Vec::new();
    let mut at = 0;
    for len in [441usize, 17, 1000].iter().cycle() {
        let end = (at + len).min(n);
        chunked.process(&input[at..end], &mut b);
        at = end;
        if at == n {
            break;
        }
    }
    assert_eq!(a, b, "chunking does not change the output");
    assert!(24_000 - a.len() < 32, "length {}", a.len());
    let body = &a[200..a.len() - 200];
    let db = 20.0 * (rms(body) / (0.5 / 2f64.sqrt())).log10();
    assert!(db.abs() < 0.3, "passband level {db} dB");
}

#[test]
fn capture_converter_emits_exact_frames_from_odd_chunks() {
    for (device_rate, channels) in [(44_100u32, 1usize), (48_000, 2), (16_000, 1)] {
        let q = VoiceQuality::Standard;
        let mut conv = CaptureConverter::new(device_rate, channels, q).expect("converter");
        let total = device_rate as usize;
        let mut fed = 0;
        let mut frames = 0;
        for len in [441usize, 17, 1000].iter().cycle() {
            let take = (*len).min(total - fed);
            conv.push(&vec![0.1f32; take * channels]);
            fed += take;
            while let Some(f) = conv.pop_frame() {
                assert_eq!(f.len(), q.frame_samples());
                frames += 1;
            }
            if fed == total {
                break;
            }
        }
        assert!(
            (49..=50).contains(&frames),
            "{device_rate} Hz x{channels}: {frames} frames"
        );
    }
    assert!(CaptureConverter::new(48_000, 0, VoiceQuality::High).is_err());
    assert!(Resampler::new(0, 48_000).is_err());
}

#[test]
fn framer_pads_instead_of_stretching_and_high_pass_removes_dc() {
    let mut f = Framer::new(4);
    f.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    assert_eq!(f.pop_frame(), Some(vec![1.0, 2.0, 3.0, 4.0]));
    assert_eq!(f.pop_frame(), None);
    assert_eq!(f.flush(), Some(vec![5.0, 6.0, 0.0, 0.0]));
    assert_eq!(f.flush(), None);
    let mut hp = HighPass::new(90.0, 16_000);
    let mut dc = vec![0.5f32; 16_000];
    hp.process(&mut dc);
    assert!(dc[15_000..].iter().all(|s| s.abs() < 1e-3), "DC is removed");
}

// ----- 5. loss and the simulator ----------------------------------------------------------

#[test]
fn simulator_is_deterministic_and_hits_its_loss_target() {
    let run = |seed| {
        let mut sim = VoiceChatSimulator::new(SimulatorSettings {
            loss_pct: 20.0,
            burst_len: 4.0,
            reorder_pct: 5.0,
            dup_pct: 5.0,
            latency_frames: 2,
            jitter_frames: 2,
            seed,
        })
        .expect("settings");
        let mut out = Vec::new();
        for n in 0..5000u32 {
            sim.advance();
            sim.send_packet(&n.to_le_bytes());
            while let Some(p) = sim.poll_packet() {
                out.push(p);
            }
        }
        (out, sim.stats())
    };
    let (a, stats) = run(7);
    let (b, _) = run(7);
    let (c, _) = run(8);
    assert_eq!(a, b);
    assert_ne!(a, c);
    let loss = stats.lost as f64 / stats.sent as f64;
    assert!((0.15..0.25).contains(&loss), "burst loss {loss}");
    assert!(stats.duplicated > 0 && stats.reordered > 0);
    assert!(
        SimulatorSettings {
            loss_pct: 96.0,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn playout_accounts_for_every_lost_frame_at_each_loss_rate() {
    for (loss, burst) in [
        (0.0f32, 1.0f32),
        (5.0, 1.0),
        (10.0, 4.0),
        (20.0, 1.0),
        (40.0, 6.0),
    ] {
        let q = VoiceQuality::Narrow;
        // Bursts reaching six frames fade to silence and re-buffer (spec § 2.9), so the books only
        // balance frame for frame for independent loss.
        let exact = burst <= 1.0;
        let mut sim = VoiceChatSimulator::new(SimulatorSettings {
            loss_pct: loss,
            burst_len: burst,
            seed: 11,
            ..Default::default()
        })
        .expect("settings");
        let mut speaker = VoiceSpeaker::new(3, JitterSettings::default());
        let total = 400u16;
        let mut seen = std::collections::BTreeSet::new();
        let mut frames = Vec::new();
        for n in 0..total {
            sim.advance();
            sim.send_packet(&pcm_packet(n, 3, 1, false, q, 0.25));
            while let Some(p) = sim.poll_packet() {
                let pk = VoicePacket::parse(&p).expect("parses");
                seen.insert(pk.seq);
                speaker.receive(&pk, u64::from(n));
            }
            frames.push(speaker.pull_frame(u64::from(n)));
        }
        let last = *seen.iter().next_back().expect("something arrived");
        let first = *seen.iter().next().expect("something arrived");
        let lost_in_range = usize::from(last - first) + 1 - seen.len();
        let end = pcm_packet(total, 3, 1, true, q, 0.25);
        speaker.receive(&VoicePacket::parse(&end).expect("parses"), u64::from(total));
        for k in 0..(total + 20) {
            frames.push(speaker.pull_frame(u64::from(total) + u64::from(k)));
        }
        let s = speaker.stats().jitter;
        let accounted = (s.recovered + s.concealed + s.silenced) as usize;
        assert!(
            !exact || accounted.abs_diff(lost_in_range) <= 2 + total as usize / 40,
            "loss {loss}%: accounted {accounted}, lost {lost_in_range}, {s:?}"
        );
        for f in frames.iter().flatten() {
            assert!(f.is_finite() && f.abs() <= 1.0);
        }
    }
}

// ----- 7. state machines ------------------------------------------------------------------

#[test]
fn transmit_machine_walks_every_transition() {
    let mut tx = TransmitMachine::new(3);
    assert_eq!(tx.state(), &TxState::Off);
    assert_eq!(tx.step(true), TxFrame::default(), "off sends nothing");
    tx.device_opened(Err(VoiceChatError::PermissionDenied));
    assert_eq!(tx.state(), &TxState::Error("permission_denied"));
    tx.device_opened(Ok(()));
    assert_eq!(tx.state(), &TxState::Idle);
    assert_eq!(tx.step(false), TxFrame::default());
    assert_eq!(
        tx.step(true),
        TxFrame {
            transmit: true,
            end_of_talk: false
        }
    );
    assert_eq!(tx.state(), &TxState::Transmitting);
    assert_eq!(
        tx.step(false),
        TxFrame {
            transmit: true,
            end_of_talk: false
        }
    );
    assert_eq!(tx.state(), &TxState::Hangover);
    assert_eq!(
        tx.step(true),
        TxFrame {
            transmit: true,
            end_of_talk: false
        }
    );
    assert_eq!(tx.state(), &TxState::Transmitting);
    assert!(!tx.step(false).end_of_talk);
    assert!(!tx.step(false).end_of_talk);
    assert_eq!(
        tx.step(false),
        TxFrame {
            transmit: true,
            end_of_talk: true
        }
    );
    assert_eq!(tx.state(), &TxState::Idle);
    tx.step(true);
    assert!(tx.set_muted(true), "mute mid-speech owes an end_of_talk");
    assert_eq!(tx.state(), &TxState::Muted);
    assert_eq!(tx.step(true), TxFrame::default());
    assert!(!tx.set_muted(false));
    assert_eq!(tx.state(), &TxState::Idle);
    tx.step(true);
    assert!(tx.device_removed());
    assert_eq!(tx.state(), &TxState::Error("no_input_device"));
    for _ in 0..RETRY_FRAMES - 1 {
        tx.retry(true);
        assert!(matches!(tx.state(), TxState::Error(_)));
    }
    tx.retry(false);
    assert!(matches!(tx.state(), TxState::Error(_)), "still no device");
    tx.retry(true);
    assert_eq!(tx.state(), &TxState::Idle);
}

#[test]
fn speaker_walks_silent_buffering_speaking_draining_and_times_out() {
    let q = VoiceQuality::Narrow;
    let mut s = VoiceSpeaker::new(5, JitterSettings::default());
    assert_eq!(s.state(), SpeakerState::Silent);
    assert!(s.pull_frame(0).is_empty());
    for n in 0..3u16 {
        let wire = pcm_packet(n, 5, 1, false, q, 0.2);
        s.receive(&VoicePacket::parse(&wire).expect("parses"), u64::from(n));
    }
    assert_eq!(s.state(), SpeakerState::Buffering);
    assert_eq!(s.pull_frame(3).len(), q.frame_samples());
    assert_eq!(s.state(), SpeakerState::Speaking);
    let end = pcm_packet(3, 5, 1, true, q, 0.2);
    s.receive(&VoicePacket::parse(&end).expect("parses"), 4);
    assert_eq!(s.state(), SpeakerState::Draining);
    for k in 0..6 {
        s.pull_frame(5 + k);
    }
    assert_eq!(s.state(), SpeakerState::Silent);
    for n in 0..4u16 {
        let wire = pcm_packet(100 + n, 5, 1, false, q, 0.2);
        s.receive(&VoicePacket::parse(&wire).expect("parses"), 100 + u64::from(n));
    }
    s.pull_frame(104);
    assert_eq!(s.state(), SpeakerState::Speaking);
    s.pull_frame(104 + SPEAKER_TIMEOUT_FRAMES + 2);
    assert_eq!(s.state(), SpeakerState::Silent);
    assert_eq!(s.stats().timeouts, 1);
}

/// A talk spurt that follows the previous one back to back, with its first frame lost, must not
/// conceal from the previous utterance's last frame (audit-gameplay-muse M3): the decoder's
/// concealment state is reset at the spurt boundary.
#[test]
fn back_to_back_spurt_does_not_conceal_from_the_previous_utterance() {
    let q = VoiceQuality::Narrow;
    let mut s = VoiceSpeaker::new(6, JitterSettings::default());
    // Spurt A: loud, seq 0..=3, ends at 3.
    for n in 0..4u16 {
        let wire = pcm_packet(n, 6, 1, n == 3, q, 0.5);
        s.receive(&VoicePacket::parse(&wire).expect("parses"), 0);
    }
    // Spurt B: quiet, its first frame (seq 4) lost, the rest already queued behind A's end.
    for n in 5..9u16 {
        let wire = pcm_packet(n, 6, 1, false, q, 0.05);
        s.receive(&VoicePacket::parse(&wire).expect("parses"), 0);
    }
    for k in 0..4 {
        let frame = s.pull_frame(k);
        assert_eq!(frame.len(), q.frame_samples(), "spurt A frame {k}");
    }
    // The lost first frame of B: concealment may give silence, never A's 0.5 level.
    let gap = s.pull_frame(4);
    assert!(
        rms(&gap) < 0.1,
        "the lost first frame of a new spurt leaked the old utterance: rms {}",
        rms(&gap)
    );
    let next = s.pull_frame(5);
    assert!(
        (rms(&next) - 0.05).abs() < 0.02,
        "spurt B plays: rms {}",
        rms(&next)
    );
}

// ----- 8. activity and push-to-talk -------------------------------------------------------

#[test]
fn vad_burst_gives_transmit_plus_hangover_then_idle() {
    let mut tx = TransmitMachine::default();
    tx.device_opened(Ok(()));
    let mut gate = TalkGate::new(VoiceMode::Vad, FrameVadSettings::default());
    let mut noise = TestSource::noise(1, -30.0);
    let mut transmitted = 0;
    let mut ended = 0;
    for n in 0..60 {
        let mut frame = vec![0.0f32; 480];
        if n < 10 {
            noise.fill(&mut frame, 24_000);
        }
        let out = tx.step(gate.wants_talk(&frame, false));
        transmitted += usize::from(out.transmit);
        ended += usize::from(out.end_of_talk);
    }
    assert_eq!(transmitted, 10 + HANGOVER_FRAMES as usize);
    assert_eq!(ended, 1);
    assert_eq!(tx.state(), &TxState::Idle);
}

#[test]
fn push_to_talk_grace_toggle_and_open_modes() {
    let silence = vec![0.0f32; 480];
    let mut ptt = TalkGate::new(VoiceMode::PushToTalk, FrameVadSettings::default());
    assert!(!ptt.wants_talk(&silence, false));
    assert!(ptt.wants_talk(&silence, true));
    let mut after = 0;
    while ptt.wants_talk(&silence, false) {
        after += 1;
    }
    assert_eq!(after, PTT_GRACE_FRAMES);
    let mut toggle = TalkGate::new(VoiceMode::Toggle, FrameVadSettings::default());
    assert!(toggle.wants_talk(&silence, true));
    assert!(toggle.wants_talk(&silence, true), "holding does not flip it back");
    assert!(toggle.wants_talk(&silence, false));
    assert!(!toggle.wants_talk(&silence, true));
    let mut open = TalkGate::new(VoiceMode::Open, FrameVadSettings::default());
    assert!(open.wants_talk(&silence, false));
    assert_eq!(VoiceMode::default(), VoiceMode::PushToTalk);
}

#[test]
fn adaptive_threshold_follows_a_raised_noise_floor() {
    let mut vad = FrameVad::new(FrameVadSettings {
        adaptive: true,
        ..Default::default()
    });
    let quiet = vad.on_threshold_db();
    let mut noise = TestSource::noise(2, -35.0);
    let mut frame = vec![0.0f32; 480];
    for _ in 0..40 {
        // Silence first so the floor learns its minimum, then steady noise raises it slowly.
        vad.process(&[0.0; 480]);
    }
    for _ in 0..400 {
        noise.fill(&mut frame, 24_000);
        vad.process(&frame);
    }
    assert!(
        vad.on_threshold_db() > quiet,
        "{} vs {quiet}",
        vad.on_threshold_db()
    );
    assert_eq!(
        FrameVad::new(FrameVadSettings::default()).on_threshold_db(),
        -40.0
    );
    assert!(frame_rms_db(&[0.0; 10]) <= -120.0);
}

// ----- 9. routing -------------------------------------------------------------------------

fn table() -> MembershipTable {
    let mut t = MembershipTable::default();
    for c in VoiceChatSettings::default().channels {
        t.channels.insert(c.id, c);
    }
    for (slot, x, chans) in [
        (1u16, 0.0f32, vec![1u16, 2, 4]),
        (2, 10.0, vec![1, 2, 4]),
        (3, 29.0, vec![2, 4]),
        (4, 31.0, vec![2, 3, 4]),
        (5, 0.0, vec![3]),
    ] {
        t.members.insert(
            slot,
            Member {
                channels: chans.into_iter().collect(),
                position: [x, 0.0, 0.0],
                ..Default::default()
            },
        );
    }
    t
}

#[test]
fn routing_by_channel_kind_radius_block_mute_and_exclusion() {
    let mut t = table();
    assert_eq!(t.recipients(1, 1), [2], "team");
    assert_eq!(t.recipients(1, 4), [2, 3, 4], "global");
    assert_eq!(
        t.recipients(1, 2),
        [2, 3],
        "proximity: 25 m + 5 m slack, so 29 m yes and 31 m no"
    );
    assert_eq!(t.recipients(4, 3), [5], "party");
    assert!(t.recipients(5, 1).is_empty(), "sender must be in the channel");
    assert!(t.recipients(1, 99).is_empty(), "unknown channel");
    t.members.get_mut(&2).expect("member").blocked.insert(1);
    assert!(t.recipients(1, 1).is_empty(), "block is mutual");
    assert!(!t.recipients(2, 4).contains(&1));
    t.members.get_mut(&2).expect("member").blocked.clear();
    t.members.get_mut(&3).expect("member").muted.insert(1);
    assert_eq!(t.recipients(1, 4), [2, 4], "a mute hint stops forwarding");
    t.members.get_mut(&2).expect("member").excluded = true;
    assert_eq!(
        t.recipients(1, 2),
        Vec::<u16>::new(),
        "excluded members leave proximity"
    );
    assert_eq!(t.recipients(1, 4), [2, 4], "but not the global channel");
}

#[test]
fn proximity_cache_keeps_distance_and_rereads_blocks() {
    let mut t = table();
    assert_eq!(t.recipients_at(1, 2, 0), t.recipients(1, 2));
    assert_eq!(t.recipients_at(1, 2, 0), [2, 3]);
    // A move inside the cache window does not change who is in range.
    t.members.get_mut(&3).expect("member").position = [1_000.0, 0.0, 0.0];
    assert_eq!(t.recipients(1, 2), [2], "the uncached walk sees the move");
    assert_eq!(
        t.recipients_at(1, 2, 50),
        [2, 3],
        "distance stays cached for 100 ms"
    );
    // A mute is not cached: it drops on the next packet.
    t.members.get_mut(&2).expect("member").muted.insert(1);
    assert_eq!(t.recipients_at(1, 2, 60), [3], "a mute is read on this call");
    // The window ends at exactly 100 ms, and the moved listener is then out of range.
    t.members.get_mut(&2).expect("member").muted.clear();
    assert_eq!(t.recipients_at(1, 2, 100), [2]);
    // Exclusion is read on the call, even while a fresh distance entry says "in range".
    t.members.get_mut(&2).expect("member").excluded = true;
    assert!(
        t.recipients_at(1, 2, 120).is_empty(),
        "an excluded member leaves proximity immediately"
    );
    assert_eq!(
        t.recipients_at(1, 4, 120),
        [2, 3, 4],
        "exclusion does not remove anyone from the global channel"
    );
}

#[test]
fn ranking_orders_by_priority_energy_and_sender_and_is_deterministic() {
    let c = |sender, priority, energy_db| Candidate {
        sender,
        priority,
        energy_db,
    };
    let cands = [
        c(9, 0, -10.0),
        c(4, 3, -40.0),
        c(2, 2, -20.0),
        c(7, 2, -20.0),
        c(1, 2, -5.0),
    ];
    assert_eq!(rank_speakers(&cands, 3), [1, 2, 4]);
    assert_eq!(rank_speakers(&cands, 4), [1, 2, 4, 7]);
    assert_eq!(rank_speakers(&cands, 99), [1, 2, 4, 7, 9]);
    assert!(rank_speakers(&[], 3).is_empty());
}

#[test]
fn ranking_hysteresis_keeps_a_near_equal_incumbent() {
    let c = |sender, energy_db| Candidate {
        sender,
        priority: 0,
        energy_db,
    };
    // Sender 2 is 1 dB quieter than challenger 1, but is in the audible set already.
    let cands = [c(1, -20.0), c(2, -21.0)];
    assert_eq!(rank_speakers(&cands, 1), [1]);
    assert_eq!(rank_speakers_sticky(&cands, 1, &[2], RANK_STAY_IN_MARGIN_DB), [2]);
    // A challenger clearly louder than the margin still wins.
    let loud = [c(1, -10.0), c(2, -21.0)];
    assert_eq!(rank_speakers_sticky(&loud, 1, &[2], RANK_STAY_IN_MARGIN_DB), [1]);
}

// ----- 11. moderation and config ----------------------------------------------------------

#[test]
fn account_lists_cap_and_evict_the_oldest() {
    let mut list = AccountList::default();
    for i in 0..ACCOUNT_LIST_CAP + 10 {
        assert!(list.add(&format!("acct{i}"), ""));
    }
    assert_eq!(list.0.len(), ACCOUNT_LIST_CAP);
    assert!(!list.contains("acct0") && list.contains("acct10"));
    assert!(list.contains(&format!("acct{}", ACCOUNT_LIST_CAP + 9)));
    assert!(!list.add("acct10", "renamed"));
    assert!(list.remove("acct10") && !list.remove("acct10"));
}

#[test]
fn user_settings_round_trip_and_default_to_privacy() {
    let mut user = VoiceChatUserSettings::default();
    assert!(!user.consent && user.mode == VoiceMode::PushToTalk && !user.deafen);
    user.muted_accounts.add("steam:1", "loud");
    user.blocked_accounts.add("steam:2", "");
    let text = ron::ser::to_string(&user).expect("serializes");
    let back: VoiceChatUserSettings = ron::de::from_str(&text).expect("parses");
    assert_eq!(back, user);
    let sparse: VoiceChatUserSettings =
        ron::de::from_str("(deafen: true)").expect("missing keys take defaults");
    assert!(sparse.deafen && !sparse.consent);
}

#[test]
fn report_ring_keeps_only_the_last_seconds_and_nothing_when_off() {
    let mut off = ReportRing::new(0);
    off.record(1, 1, &[1, 2, 3]);
    assert!(off.is_empty() && off.report(1).is_none());
    let mut ring = ReportRing::new(1);
    for i in 0..120u8 {
        ring.record(1, 4, &[i]);
    }
    assert_eq!(ring.len(1), 50, "one second of 50 packets/s");
    let r = ring.report(1).expect("report");
    assert_eq!(r.channel, 4);
    assert_eq!(
        (r.packets.first().cloned(), r.packets.last().cloned()),
        (Some(vec![70]), Some(vec![119]))
    );
    ring.clear();
    assert!(ring.is_empty());
}

/// Spec 59 § 2.4 and § 2.13 limits at both edges: `max_mixed_speakers` 1..=16 and
/// `report_buffer_seconds` 0..=30 (hard cap 30).
#[test]
fn config_limits_hold_at_both_edges() {
    let with = |speakers: u8, report: u32| VoiceChatSettings {
        max_mixed_speakers: speakers,
        report_buffer_seconds: report,
        ..VoiceChatSettings::default()
    };
    for ok in [with(1, 0), with(16, 30)] {
        ok.validate().expect("edge values are valid");
    }
    for bad in [with(0, 0), with(17, 0), with(8, 31)] {
        let e = bad.validate().expect_err("out of range");
        assert_eq!(e.code(), "invalid_params");
    }
}

#[test]
fn config_defaults_match_the_spec_and_the_version_gate_works() {
    let s = VoiceChatSettings::default();
    s.validate().expect("defaults are valid");
    assert_eq!(
        (s.mode, s.max_mixed_speakers, s.frames_per_packet),
        (VoiceMode::PushToTalk, 8, 1)
    );
    assert_eq!(s.channels.len(), 4);
    let text = s.to_ron().expect("serializes");
    assert_eq!(VoiceChatSettings::from_ron(&text).expect("parses"), s);
    let spec = r#"(format_version: 1, quality: Standard, mode: PushToTalk, frames_per_packet: 1, max_mixed_speakers: 8,
        jitter: (min_ms: 40, max_ms: 200, start_ms: 60), processing: (noise_suppression: Off, echo_cancel: false, agc: false),
        report_buffer_seconds: 0, channels: [(id: 1, name: "team", kind: Team, priority: 3),
        (id: 2, name: "nearby", kind: Proximity, radius: 25.0, spatial: true, priority: 2),
        (id: 3, name: "party", kind: Party, priority: 1), (id: 4, name: "all", kind: Global, priority: 0)])"#;
    assert_eq!(
        VoiceChatSettings::from_ron(spec)
            .expect("the spec's own example parses")
            .channels
            .len(),
        4
    );
    assert!(VoiceChatSettings::from_ron(&text.replace("format_version: 1", "format_version: 2")).is_err());
    assert!(
        VoiceChatSettings {
            max_mixed_speakers: 17,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    let mut dup = VoiceChatSettings::default();
    dup.channels[1].id = 1;
    assert!(dup.validate().is_err());
}

#[test]
fn every_error_code_is_documented_sorted_and_lowercase() {
    let mut sorted = ERROR_CODES;
    sorted.sort_unstable();
    assert_eq!(sorted, ERROR_CODES);
    for code in ERROR_CODES {
        assert!(
            code.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
    }
    let all = [
        VoiceChatError::ConsentRequired,
        VoiceChatError::PermissionDenied,
        VoiceChatError::NoInputDevice,
        VoiceChatError::DeviceBusy,
        VoiceChatError::UnsupportedFormat(String::new()),
        VoiceChatError::Codec(String::new()),
        VoiceChatError::BadPacket(String::new()),
        VoiceChatError::UnknownChannel(0),
        VoiceChatError::NotInChannel(0),
        VoiceChatError::UnknownSpeaker(0),
        VoiceChatError::InvalidParams(String::new()),
        VoiceChatError::Unsupported(String::new()),
        VoiceChatError::ServerMuted,
        VoiceChatError::RateLimited,
        VoiceChatError::TestModeOnly,
    ];
    let mut codes: Vec<&str> = all.iter().map(VoiceChatError::code).collect();
    codes.sort_unstable();
    assert_eq!(codes, ERROR_CODES);
}

// ----- the session ------------------------------------------------------------------------

struct MockOpener {
    opens: u32,
    result: Result<InputDevice, VoiceChatError>,
}

impl InputDeviceOpener for MockOpener {
    fn open(&mut self, _name: Option<&str>) -> Result<InputDevice, VoiceChatError> {
        self.opens += 1;
        self.result.clone()
    }
}

fn session() -> VoiceChatSession {
    let settings = VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        ..Default::default()
    };
    VoiceChatSession::new(settings, VoiceChatUserSettings::default()).expect("session")
}

#[test]
fn without_consent_the_device_is_never_opened() {
    let mut s = session();
    let mut opener = MockOpener {
        opens: 0,
        result: Ok(InputDevice {
            name: "mock".into(),
            rate: 48_000,
            channels: 1,
        }),
    };
    assert_eq!(s.start_capture(&mut opener), Err(VoiceChatError::ConsentRequired));
    assert_eq!(opener.opens, 0);
    assert!(
        s.set_loopback(true, true).is_err(),
        "mic loopback needs consent too"
    );
    s.set_loopback(true, false)
        .expect("synthetic loopback needs none");
    s.user_mut().consent = true;
    s.start_capture(&mut opener).expect("opens");
    assert_eq!(opener.opens, 1);
    assert_eq!(s.tx_state(), &TxState::Idle);
    let mut denied = MockOpener {
        opens: 0,
        result: Err(VoiceChatError::PermissionDenied),
    };
    let mut s2 = session();
    s2.user_mut().consent = true;
    assert_eq!(
        s2.start_capture(&mut denied),
        Err(VoiceChatError::PermissionDenied)
    );
    assert_eq!(s2.status().error, Some("permission_denied"));
}

#[test]
fn a_fake_remote_speaker_is_heard_through_the_whole_receive_path() {
    let mut s = session();
    s.set_channel(2, Some(true), Some(true), None).expect("join");
    s.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
    let mut heard = Vec::new();
    for _ in 0..40 {
        s.tick(false);
        for (sender, rate, samples) in s.playout() {
            assert_eq!((*sender, *rate), (7, 16_000));
            heard.extend_from_slice(samples);
        }
    }
    let st = s.status();
    assert_eq!(st.speakers.len(), 1);
    assert_eq!(st.speakers[0].state, "speaking");
    assert!(heard.len() >= 30 * 320);
    let tail = &heard[heard.len() - 1600..];
    assert!(tone_power(tail, 440.0, 16_000) > 20.0 * tone_power(tail, 1000.0, 16_000));
    assert!(s.take_events().iter().any(|e| matches!(
        e,
        VoiceChatEvent::SpeakerActivity {
            sender: 7,
            active: true,
            ..
        }
    )));
    let mut other = session();
    other.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
    other.tick(false);
    other.tick(false);
    assert!(other.stats().wrong_channel > 0 && other.playout().is_empty());
}

/// Spec 59 § 2.3: a sender id reused by a different account resets that speaker (decoder and
/// buffer discarded) when the rebinding is announced; the same account again changes nothing.
#[test]
fn a_sender_slot_rebound_to_another_account_resets_its_speaker() {
    let mut s = session();
    s.set_channel(2, Some(true), Some(true), None).expect("join");
    s.bind_sender(7, "steam:1");
    s.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
    for _ in 0..20 {
        s.tick(false);
    }
    let state = |s: &VoiceChatSession| s.speaker(7).expect("speaker").state();
    assert_eq!(state(&s), SpeakerState::Speaking);
    s.bind_sender(7, "steam:1");
    assert_eq!(
        state(&s),
        SpeakerState::Speaking,
        "the same account keeps the speaker"
    );
    s.bind_sender(7, "steam:2");
    assert_eq!(
        state(&s),
        SpeakerState::Silent,
        "a new account starts from nothing"
    );
    assert_eq!(s.speaker(7).expect("speaker").depth_frames(), 0);
}

/// Spec 59 § 2.4: at most `max_mixed_speakers` speakers are mixed at once. A speaker that lost
/// the ranking used to be reset every 10 frames and then re-buffered and played between the
/// re-rankings, so a 1-speaker cap mixed two speakers for most of every window (ADR-5642).
#[test]
fn the_mixing_cap_holds_between_re_rankings() {
    let settings = VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        max_mixed_speakers: 1,
        ..Default::default()
    };
    let mut s = VoiceChatSession::new(settings, VoiceChatUserSettings::default()).expect("session");
    s.set_channel(2, Some(true), Some(true), None).expect("join");
    s.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
    s.set_fake_speaker(9, Some(TestSource::sine(660.0, -6.0)));
    let mut heard_frames = 0;
    let mut speakers_heard = std::collections::BTreeSet::new();
    for frame in 0..200 {
        s.tick(false);
        let mixed = s.playout().len();
        assert!(
            mixed <= 1,
            "frame {frame}: {mixed} speakers mixed with a cap of 1"
        );
        if let Some((sender, _, _)) = s.playout().first() {
            heard_frames += 1;
            speakers_heard.insert(*sender);
        }
    }
    assert!(
        heard_frames > 150,
        "the kept speaker plays: {heard_frames} frames"
    );
    assert_eq!(
        speakers_heard.len(),
        1,
        "the audible set does not flap: {speakers_heard:?}"
    );
    assert!(s.stats().overflow_drops > 0);
}

/// Spec 59 § 2.4 and § 2.14: a speaker that starts while the audible set is full, and is let in
/// at a later re-ranking, reports its "started" `SpeakerActivity` edge no later than the first
/// frame it is heard. The full-set skip used to run before the activity edge was taken, so the
/// start was swallowed and a game's speaking icon got only the "stopped" edge. Every ranking
/// phase (the frame within the 10-frame window when the talk turn changes) is covered.
#[test]
fn a_speaker_admitted_from_a_full_set_reports_its_start_before_it_is_heard() {
    let q = VoiceQuality::Narrow;
    let mut bad = Vec::new();
    for phase in 0..10u16 {
        let settings = VoiceChatSettings {
            deterministic: true,
            quality: q,
            max_mixed_speakers: 1,
            ..Default::default()
        };
        let mut s = VoiceChatSession::new(settings, VoiceChatUserSettings::default()).expect("session");
        s.set_channel(2, Some(true), Some(true), None).expect("join");
        s.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
        for _ in 0..(40 + phase) {
            s.tick(false);
        }
        let _ = s.take_events();
        let mut edges = Vec::new();
        let mut first_heard = None;
        for n in 0..80u16 {
            if n < 40 {
                s.receive_packet(&pcm_packet(n, 8, 2, n == 39, q, 0.2));
            }
            if n == 5 {
                s.set_fake_speaker(7, None);
            }
            s.tick(false);
            if first_heard.is_none() && s.playout().iter().any(|(sender, _, _)| *sender == 8) {
                first_heard = Some(n);
            }
            for event in s.take_events() {
                if let VoiceChatEvent::SpeakerActivity {
                    sender: 8, active, ..
                } = event
                {
                    edges.push((n, active));
                }
            }
        }
        let first_start = edges.iter().find(|(_, active)| *active).map(|(n, _)| *n);
        let ok = first_heard.is_some()
            && edges.first().is_some_and(|(_, active)| *active)
            && first_start <= first_heard
            && edges.last().is_some_and(|(_, active)| !*active);
        if !ok {
            bad.push((phase, first_heard, edges));
        }
    }
    assert!(
        bad.is_empty(),
        "(phase, first frame heard, edges) without a start before hearing: {bad:?}"
    );
}

#[test]
fn loopback_runs_the_whole_chain_and_turns_itself_off() {
    let mut s = session();
    s.set_channel(1, Some(true), Some(true), None).expect("join");
    s.set_test_source(Some(TestSource::sine(300.0, -6.0)));
    s.set_loopback(true, false).expect("on");
    for _ in 0..20 {
        s.tick(true);
    }
    let st = s.status();
    assert_eq!(st.state, "transmitting");
    assert!(st.loopback && st.test_mode);
    assert!(
        st.speakers
            .iter()
            .any(|sp| sp.sender == LOOPBACK_SENDER && sp.state == "speaking")
    );
    assert!(s.stats().packets_sent >= 19);
    assert_eq!(s.take_outgoing().len() as u64, s.stats().packets_sent);
    for _ in 0..LOOPBACK_MAX_FRAMES {
        s.tick(true);
    }
    assert!(!s.loopback(), "auto-disables after 3000 frames");
    assert!(s.status().speakers.iter().all(|sp| sp.sender != LOOPBACK_SENDER));
}

#[test]
fn local_mute_block_and_deafen_drop_before_decode() {
    let mut s = session();
    s.set_channel(4, Some(true), Some(true), None).expect("join");
    s.bind_sender(7, "steam:7");
    s.mute_account("steam:7", "chatty", true);
    s.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
    for _ in 0..10 {
        s.tick(false);
    }
    assert!(s.stats().muted_drops >= 9 && s.playout().is_empty());
    assert_eq!(s.user().muted_accounts.0.len(), 1);
    s.mute_account("steam:7", "", false);
    s.reset_stats();
    for _ in 0..10 {
        s.tick(false);
    }
    assert_eq!(s.stats().muted_drops, 0);
    assert!(!s.playout().is_empty());
    s.block_account("steam:7", "", true);
    assert!(s.is_sender_silenced(7));
    s.block_account("steam:7", "", false);
    s.set_deafened(true);
    s.reset_stats();
    s.tick(false);
    assert!(s.stats().deafened_drops > 0 && s.playout().is_empty());
    assert!(s.set_speaker_volume(7, 250.0).is_err());
    assert!(s.set_speaker_volume(99, 100.0).is_err());
}

#[test]
fn channel_errors_use_the_documented_codes() {
    let mut s = session();
    assert_eq!(
        s.set_channel(99, Some(true), None, None).err().map(|e| e.code()),
        Some("unknown_channel")
    );
    assert_eq!(
        s.set_channel(1, None, Some(true), None).err().map(|e| e.code()),
        Some("not_in_channel")
    );
    assert_eq!(
        s.set_channel(2, Some(true), None, Some(-1.0))
            .err()
            .map(|e| e.code()),
        Some("invalid_params")
    );
    assert_eq!(
        s.set_channel(2, Some(true), Some(true), Some(40.0)).expect("ok"),
        (true, true)
    );
    assert_eq!(s.settings().channels[1].radius, 40.0);
    assert_eq!(
        s.set_channel(2, Some(false), None, None).expect("ok"),
        (false, false)
    );
    assert_eq!(s.active_channel(), None);
}

struct CollectSink(Vec<VoiceReport>);

impl VoiceReportSink for CollectSink {
    fn submit(&mut self, report: VoiceReport) {
        self.0.push(report);
    }
}

#[test]
fn reports_hand_over_only_buffered_encoded_packets_and_clear_at_session_end() {
    let settings = VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        report_buffer_seconds: 1,
        ..Default::default()
    };
    let mut s = VoiceChatSession::new(settings, VoiceChatUserSettings::default()).expect("session");
    s.set_channel(4, Some(true), Some(true), None).expect("join");
    s.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
    for _ in 0..80 {
        s.tick(false);
    }
    let mut sink = CollectSink(Vec::new());
    assert!(s.report(7, &mut sink));
    assert_eq!(sink.0[0].packets.len(), 50);
    assert!(!s.report(8, &mut sink));
    s.end_session();
    assert!(!s.report(7, &mut sink), "the ring is cleared at session end");
    let mut off = session();
    off.set_channel(4, Some(true), Some(true), None).expect("join");
    off.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
    for _ in 0..10 {
        off.tick(false);
    }
    assert!(!off.report(7, &mut sink));
}

#[test]
fn bandwidth_counts_are_exact_for_pcm_and_loss_conceals_through_the_session() {
    let mut s = session();
    s.set_channel(1, Some(true), Some(true), None).expect("join");
    s.set_test_source(Some(TestSource::sine(440.0, -6.0)));
    for _ in 0..500 {
        s.tick(true);
    }
    let per_packet = (HEADER_LEN + 1 + 2 * VoiceQuality::Narrow.frame_samples()) as u64;
    let st = s.stats();
    assert_eq!(st.packets_sent, 500);
    assert_eq!(st.bytes_sent, 500 * per_packet);
    let mut lossy = session();
    lossy.set_channel(1, Some(true), Some(true), None).expect("join");
    lossy
        .simulate_network(SimulatorSettings {
            loss_pct: 20.0,
            burst_len: 3.0,
            jitter_frames: 2,
            seed: 3,
            ..Default::default()
        })
        .expect("settings");
    lossy.set_fake_speaker(9, Some(TestSource::sine(440.0, -6.0)));
    for _ in 0..300 {
        lossy.tick(false);
        for (_, _, f) in lossy.playout() {
            assert!(f.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
    }
    let stats = lossy.speaker_stats();
    assert!(stats[0].1.jitter.concealed + stats[0].1.jitter.recovered > 0);
    let bad = SimulatorSettings {
        loss_pct: -1.0,
        ..Default::default()
    };
    assert_eq!(
        VoiceChatSimulator::new(bad).err().map(|e| e.code()),
        Some("invalid_params")
    );
}

#[test]
fn transport_pump_moves_packets_between_two_sessions() {
    let mut a = session();
    a.set_local_sender(1);
    a.set_channel(1, Some(true), Some(true), None).expect("join");
    a.set_test_source(Some(TestSource::sine(440.0, -6.0)));
    let mut b = session();
    b.set_channel(1, Some(true), None, None).expect("join");
    let mut wire = VoiceChatSimulator::new(SimulatorSettings::default()).expect("settings");
    for _ in 0..30 {
        a.tick(true);
        wire.advance();
        for p in a.take_outgoing() {
            wire.send(&p);
        }
        pump_transport(&mut b, &mut wire);
        b.tick(false);
    }
    let heard = b.status().speakers;
    assert_eq!(heard.len(), 1);
    assert_eq!((heard[0].sender, heard[0].state), (1, "speaking"));
}

/// audit-gameplay (Medium): a hand-edited `output_gain_db: NaN` (or infinite) pushed NaN or
/// infinite samples for every speaker into the playout.
#[test]
fn a_non_finite_user_gain_plays_at_unity() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut reference = session();
        let mut s = session();
        for session in [&mut reference, &mut s] {
            session
                .set_channel(4, Some(true), Some(true), None)
                .expect("join");
            session.bind_sender(7, "steam:7");
            session.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
        }
        s.user_mut().output_gain_db = bad;
        s.user_mut().input_gain_db = bad;
        for _ in 0..10 {
            reference.tick(false);
            s.tick(false);
        }
        assert!(!s.playout().is_empty(), "{bad}");
        assert_eq!(s.playout(), reference.playout(), "{bad}: unity gain");
        assert!(
            s.playout()
                .iter()
                .all(|(_, _, samples)| samples.iter().all(|x| x.is_finite())),
            "{bad}"
        );
    }
    assert_eq!(regia_audio::voice_chat::user_gain(0.0), 1.0);
    assert!(regia_audio::voice_chat::user_gain(1.0e6).is_finite());
    assert!(regia_audio::voice_chat::user_gain(-1.0e6) > 0.0);
}

// ----- audit regressions (edge review 11-02) ----------------------------------------------

#[test]
fn a_non_finite_member_position_is_out_of_every_proximity_radius() {
    let mut t = table();
    let base = t.recipients(1, 2);
    assert!(base.contains(&2), "precondition: slot 2 is in range");
    t.members.get_mut(&2).expect("slot 2").position = [f32::NAN, 0.0, 0.0];
    assert!(
        !t.recipients(1, 2).contains(&2),
        "NaN listener must not hear proximity"
    );
    assert!(t.recipients(2, 2).is_empty(), "NaN speaker must not reach anyone");
    t.members.get_mut(&2).expect("slot 2").position = [f32::INFINITY, 0.0, 0.0];
    assert!(!t.recipients(1, 2).contains(&2));
}

#[test]
fn report_ring_bounds_by_timestamp_span_not_packet_count() {
    // Paired-frame packets (40 ms apart) at a 1 s ring: 25 packets cover one second, not 50.
    let mut ring = ReportRing::new(1);
    for i in 0..100u32 {
        let mut p = vec![0u8; HEADER_LEN + 2];
        p[7..11].copy_from_slice(&(i * 1920).to_le_bytes());
        ring.record(1, 4, &p);
    }
    assert!(
        ring.len(1) <= 26,
        "kept {} packets, expected about one second",
        ring.len(1)
    );
    assert!(ring.len(1) >= 24);
}

#[test]
fn end_of_talk_keeps_packets_of_the_next_utterance_already_queued() {
    let mut jb = JitterBuffer::new(JitterSettings::default());
    for seq in 0..3u16 {
        jb.insert(seq, u32::from(seq) * 960, seq == 2, &[seq as u8], u64::from(seq));
    }
    // The next utterance starts right behind the end frame, before the buffer played it out.
    for seq in 3..8u16 {
        jb.insert(seq, u32::from(seq) * 960, false, &[seq as u8], u64::from(seq));
    }
    let mut played = Vec::new();
    for _ in 0..40 {
        if let Playout::Frame(p) = jb.pull() {
            played.push(p[0]);
        }
    }
    assert_eq!(
        played,
        [0, 1, 2, 3, 4, 5, 6, 7],
        "no frame of the next utterance may be dropped"
    );
}

#[test]
fn open_mic_silence_flushes_a_held_odd_frame_on_its_own_sequence() {
    let settings = VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        frames_per_packet: 2,
        ..Default::default()
    };
    let user = VoiceChatUserSettings {
        mode: VoiceMode::Open,
        ..Default::default()
    };
    let mut s = VoiceChatSession::new(settings, user).expect("session");
    s.set_channel(1, Some(true), Some(true), None).expect("join");
    s.set_test_source(Some(TestSource::sine(300.0, -6.0)));
    // Three speech frames: one pair (seq 0) and one frame held for a partner (seq 2).
    for _ in 0..3 {
        s.tick(false);
    }
    s.set_test_source(Some(TestSource::sine(300.0, -100.0)));
    for _ in 0..4 {
        s.tick(false);
    }
    let single = 1 + 2 * VoiceQuality::Narrow.frame_samples();
    let packets = s.take_outgoing();
    let lone = packets
        .iter()
        .filter_map(|w| VoicePacket::parse(w).ok())
        .find(|p| p.seq == 2)
        .expect("the held frame is sent during the pause");
    assert_eq!(
        lone.payload.len(),
        single,
        "sent alone, not paired with a later frame"
    );
}

/// The pump runs several voice ticks per game update below 50 fps (or after a hitch), and the
/// output system reads `played()` once per update. Every tick's frames must still be there: an
/// earlier version overwrote the output each tick, so at 30 fps about 40 % of voice frames were
/// never heard.
#[test]
fn every_frame_of_several_ticks_per_update_reaches_the_output() {
    let mut s = session();
    s.set_output_tap(true);
    s.set_channel(2, Some(true), Some(true), None).expect("join");
    s.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
    for _ in 0..40 {
        s.tick(false);
        s.acknowledge_output();
    }
    // Steady: one 20 ms frame per tick from speaker 7.
    s.tick(false);
    assert_eq!(s.played().len(), 1, "one frame per tick once steady");
    s.acknowledge_output();
    // Three ticks in one update, one read.
    for _ in 0..3 {
        s.tick(false);
    }
    let played = s.played();
    assert_eq!(played.len(), 3, "the three ticks' frames, in play order");
    assert!(
        played
            .iter()
            .all(|(sender, rate, samples)| *sender == 7 && *rate == 16_000 && samples.len() == 320)
    );
    assert_ne!(
        played[0].2, played[1].2,
        "distinct frames, not one frame repeated"
    );
    s.acknowledge_output();
    assert!(s.played().is_empty(), "acknowledged");
    s.tick(false);
    assert_eq!(s.take_output().len(), 1, "take_output returns the pending frames");
    assert!(s.played().is_empty());
}

/// A speaker outside the mix that starts and ends a short spurt between two rankings reports
/// both edges: "started" and "stopped". An earlier version judged "outside the mix and silent"
/// after pulling the frame, so the frame on which the speaker fell silent was skipped with its
/// "stopped" edge, and the talking indicator stayed on. Every phase of the 10-frame ranking cycle
/// is tried. The spurt arrives as a sender sends it, one packet per voice frame.
#[test]
fn a_short_spurt_between_rankings_reports_both_edges() {
    let q = VoiceQuality::Narrow;
    for phase in 0..10u16 {
        let mut s = session();
        s.set_channel(2, Some(true), Some(true), None).expect("join");
        s.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
        for _ in 0..(40 + phase) {
            s.tick(false);
        }
        let _ = s.take_events();
        for n in 0..4u16 {
            s.receive_packet(&pcm_packet(n, 8, 2, n == 3, q, 0.2));
            s.tick(false);
        }
        for _ in 0..20 {
            s.tick(false);
        }
        let edges: Vec<bool> = s
            .take_events()
            .iter()
            .filter_map(|event| match event {
                VoiceChatEvent::SpeakerActivity {
                    sender: 8, active, ..
                } => Some(*active),
                _ => None,
            })
            .collect();
        assert_eq!(
            edges,
            [true, false],
            "phase {phase}: the spurt starts and stops once"
        );
    }
}

/// A codec/quality switch sets `pending_reset`; with `frames_per_packet == 2` the first
/// post-reset frame is held for its partner, and the pair that later carries it must still
/// announce RESET — otherwise the remote decoder decodes post-reset bytes against its stale
/// state (correctness audit finding 3).
#[test]
fn a_reset_survives_the_two_frame_hold() {
    let settings = VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        frames_per_packet: 2,
        ..Default::default()
    };
    let user = VoiceChatUserSettings {
        mode: VoiceMode::Open,
        ..Default::default()
    };
    let mut s = VoiceChatSession::new(settings, user).expect("session");
    s.set_channel(1, Some(true), Some(true), None).expect("join");
    s.set_test_source(Some(TestSource::sine(300.0, -6.0)));
    // Before the first frame: a bandwidth switch rebuilds the encoder and asks for a reset.
    s.set_quality(VoiceQuality::High);
    s.tick(false);
    assert!(
        s.take_outgoing().is_empty(),
        "the first frame waits for its partner"
    );
    s.tick(false);
    let packets = s.take_outgoing();
    assert_eq!(packets.len(), 1, "the pair goes out");
    let pair = VoicePacket::parse(&packets[0]).expect("parses");
    assert!(pair.flags.contains(VoiceFlags::MULTI));
    assert!(
        pair.flags.contains(VoiceFlags::RESET),
        "the held pair still announces the encoder restart"
    );
    // The flag is consumed by the packet that carried it.
    s.tick(false);
    s.tick(false);
    let packets = s.take_outgoing();
    assert_eq!(packets.len(), 1);
    let pair = VoicePacket::parse(&packets[0]).expect("parses");
    assert!(
        !pair.flags.contains(VoiceFlags::RESET),
        "no second reset without a new trigger"
    );
}

/// A held post-reset frame that never gets its partner (the player went quiet) is flushed on
/// its own sequence number — still carrying RESET (correctness audit finding 3).
#[test]
fn a_flushed_held_frame_keeps_the_reset_flag() {
    let settings = VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        frames_per_packet: 2,
        ..Default::default()
    };
    let user = VoiceChatUserSettings {
        mode: VoiceMode::Open,
        ..Default::default()
    };
    let mut s = VoiceChatSession::new(settings, user).expect("session");
    s.set_channel(1, Some(true), Some(true), None).expect("join");
    s.set_test_source(Some(TestSource::sine(300.0, -6.0)));
    s.set_quality(VoiceQuality::High);
    s.tick(false);
    // Silence before the partner arrives: the held frame is flushed by the comfort path.
    s.set_test_source(Some(TestSource::sine(300.0, -100.0)));
    s.tick(false);
    let packets = s.take_outgoing();
    let lone = packets
        .iter()
        .filter_map(|w| VoicePacket::parse(w).ok())
        .find(|p| !p.flags.contains(VoiceFlags::MULTI))
        .expect("the held frame is flushed alone");
    assert!(
        lone.flags.contains(VoiceFlags::RESET),
        "the flushed frame still announces the encoder restart"
    );
}

// ----- post-v0.5: the § 2.14 debounce and the § 2.9 concealment fade --------------------------

/// Spec 59 § 2.14: `SpeakerActivity` is debounced to 10 Hz. Per sender, two events are at least
/// five voice frames (100 ms) apart; edges inside the window are folded into the state at its end,
/// so the events still alternate, start with "started" and end with "stopped".
#[test]
fn speaker_activity_is_debounced_to_ten_hertz() {
    let q = VoiceQuality::Narrow;
    let mut bad = Vec::new();
    for gap in 0..6usize {
        let mut s = session();
        s.set_channel(2, Some(true), Some(true), None).expect("join");
        // Three talk spurts of four frames, `gap` silent frames apart, then a long tail.
        let mut script: Vec<Option<bool>> = Vec::new();
        for _ in 0..3 {
            for k in 0..4 {
                script.push(Some(k == 3));
            }
            script.extend(std::iter::repeat_n(None, gap));
        }
        script.extend(std::iter::repeat_n(None, 40));
        let mut seq = 0u16;
        let mut edges: Vec<(usize, bool)> = Vec::new();
        for (tick, step) in script.iter().enumerate() {
            if let Some(end) = step {
                s.receive_packet(&pcm_packet(seq, 8, 2, *end, q, 0.2));
                seq = seq.wrapping_add(1);
            }
            s.tick(false);
            for event in s.take_events() {
                if let VoiceChatEvent::SpeakerActivity {
                    sender: 8, active, ..
                } = event
                {
                    edges.push((tick, active));
                }
            }
        }
        let spaced = edges.windows(2).all(|w| w[1].0 >= w[0].0 + 5);
        let alternate = edges.windows(2).all(|w| w[1].1 != w[0].1);
        let ends = edges.first().is_some_and(|e| e.1) && edges.last().is_some_and(|e| !e.1);
        if !(spaced && alternate && ends) {
            bad.push((gap, edges));
        }
    }
    assert!(
        bad.is_empty(),
        "(gap, [(tick, active)]) with events closer than 5 frames, not alternating or not closed: {bad:?}"
    );
}

/// Spec 59 § 2.9 (3): after five consecutive concealed frames the next frame fades the
/// concealment out (from its level at the first sample to zero at the last), and silence follows.
#[test]
fn the_frame_after_five_concealed_fades_out_and_silence_follows() {
    let q = VoiceQuality::Narrow;
    let n = q.frame_samples();
    let mut s = VoiceSpeaker::new(8, JitterSettings::default());
    let mut frames = Vec::new();
    for tick in 0..40u64 {
        if tick < 10 {
            let wire = pcm_packet(tick as u16, 8, 2, false, q, 0.5);
            s.receive(&VoicePacket::parse(&wire).expect("parses"), tick);
        }
        frames.push(s.pull_frame(tick));
    }
    let full = |f: &Vec<f32>| f.len() == n && (f[0] - 0.5).abs() < 0.01 && (f[n - 1] - 0.5).abs() < 0.01;
    let last = frames.iter().rposition(full).expect("the talk spurt played");
    for (k, frame) in frames[last + 1..=last + 5].iter().enumerate() {
        assert!(
            frame.len() == n && frame.iter().all(|x| *x > 0.0),
            "concealed frame {} is the faded repeat",
            k + 1
        );
    }
    let fade = &frames[last + 6];
    assert_eq!(fade.len(), n);
    assert!(
        fade[0] > 0.04,
        "the fade starts at the concealment level, got {} (a step to silence)",
        fade[0]
    );
    assert!(fade.windows(2).all(|w| w[1] <= w[0]), "the fade never rises");
    assert!(
        fade[n - 1] <= fade[0] / n as f32 * 2.0,
        "the fade ends at zero: {} of {}",
        fade[n - 1],
        fade[0]
    );
    for frame in &frames[last + 7..] {
        assert!(frame.iter().all(|x| *x == 0.0), "silence after the fade");
    }
}
