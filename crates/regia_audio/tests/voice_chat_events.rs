// SPDX-License-Identifier: MPL-2.0
//! Voice chat events and the stream error record (`docs/specs/59-voice-chat.md` §§ 2.14, 5):
//! `DecodedFrame` (opt-in `emit_decoded_frames`) and the codes a running session reports without
//! an agent call to refuse. No device, no wall clock.

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

fn session() -> VoiceChatSession {
    let settings = VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        ..Default::default()
    };
    VoiceChatSession::new(settings, VoiceChatUserSettings::default()).expect("session")
}

fn decoded(events: Vec<VoiceChatEvent>) -> Vec<(u16, u16, u32, std::sync::Arc<[f32]>)> {
    events
        .into_iter()
        .filter_map(|event| match event {
            VoiceChatEvent::DecodedFrame {
                sender,
                channel,
                rate,
                samples,
            } => Some((sender, channel, rate, samples)),
            _ => None,
        })
        .collect()
}

/// Spec 59 § 2.14: `DecodedFrame` carries each heard speaker's decoded mono PCM per 20 ms frame,
/// only while `emit_decoded_frames` is on, and at most `MAX_PENDING_DECODED_FRAMES` wait for a game
/// that never drains the events.
#[test]
fn decoded_frames_reach_the_game_only_when_opted_in() {
    let mut s = session();
    s.set_channel(2, Some(true), Some(true), None).expect("join");
    s.set_fake_speaker(7, Some(TestSource::sine(440.0, -6.0)));
    for _ in 0..20 {
        s.tick(false);
    }
    assert!(decoded(s.take_events()).is_empty(), "off by default");
    assert!(!s.emit_decoded_frames());
    s.set_emit_decoded_frames(true);
    for _ in 0..10 {
        s.tick(false);
    }
    let frames = decoded(s.take_events());
    assert_eq!(frames.len(), 10, "one frame per tick for the one speaker");
    for (sender, channel, rate, samples) in &frames {
        assert_eq!((*sender, *channel, *rate), (7, 2, 16_000));
        assert_eq!(samples.len(), 320);
    }
    let pcm: Vec<f32> = frames.iter().flat_map(|f| f.3.iter().copied()).collect();
    assert!(
        tone_power(&pcm, 440.0, 16_000) > 20.0 * tone_power(&pcm, 1000.0, 16_000),
        "the frames are the decoded 440 Hz speech"
    );
    // The samples are those the session plays (unity output gain).
    assert_eq!(&frames[9].3[..], &s.playout()[0].2[..]);
    for _ in 0..(MAX_PENDING_DECODED_FRAMES + 20) {
        s.tick(false);
    }
    assert_eq!(
        decoded(s.take_events()).len(),
        MAX_PENDING_DECODED_FRAMES,
        "an undrained queue is capped"
    );
    s.set_emit_decoded_frames(false);
    for _ in 0..5 {
        s.tick(false);
    }
    assert!(decoded(s.take_events()).is_empty(), "off again");
}

/// Spec 59 § 5: `bad_packet`, `codec_error` and `server_muted` happen to a running session, not to an
/// agent call, so the session keeps a record per code (count and last frame) that
/// `voice_chat.status` reports; `reset_stats` clears it.
#[test]
fn stream_errors_are_recorded_per_code() {
    let mut s = session();
    s.set_channel(2, Some(true), Some(true), None).expect("join");
    for _ in 0..3 {
        s.receive_packet(&[0xFF, 1, 2]);
    }
    // A well-formed header around a PCM payload in an unknown mode: the codec refuses it.
    for seq in 0..4u16 {
        let mut wire = Vec::new();
        VoicePacket {
            flags: VoiceFlags::default(),
            sender: 8,
            channel: 2,
            seq,
            timestamp: u32::from(seq).wrapping_mul(FRAME_TIMESTAMP_STEP),
            quality: VoiceQuality::Narrow,
            codec: VoiceCodecId::Pcm,
            payload: &[0x7F, 0, 0],
        }
        .write(&mut wire);
        s.receive_packet(&wire);
    }
    for _ in 0..8 {
        s.tick(false);
    }
    s.note_server_muted(1234);
    let errors = s.status().errors;
    let codes: Vec<&str> = errors.iter().map(|e| e.code).collect();
    assert_eq!(
        codes,
        ["bad_packet", "codec_error", "server_muted"],
        "sorted by code"
    );
    assert_eq!(errors[0].count, 3);
    assert!(errors[1].count >= 1);
    assert_eq!(errors[2].count, 1);
    assert!(errors.iter().all(|e| ERROR_CODES.contains(&e.code)));
    assert!(
        s.take_events()
            .contains(&VoiceChatEvent::Error(VoiceChatError::ServerMuted)),
        "a server mute is also an event for the game"
    );
    s.reset_stats();
    assert!(s.status().errors.is_empty());
}
