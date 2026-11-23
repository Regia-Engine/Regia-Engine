// SPDX-License-Identifier: MPL-2.0
//! The whole voice path with nothing real in it: a fake microphone at a device's rate, capture
//! conversion, Opus, the relay over the loopback net hub, Opus again, the jitter buffer and playout
//! (`docs/specs/59-voice-chat.md` § 8; ADR-1911).
#![cfg(feature = "voice-opus")]

use regia_audio::voice_chat::*;
use regia_ecs::prelude::*;
use regia_media::voice_opus::opus_codecs;
use regia_net::model::NetMode;
use regia_net::testing::{LinkConditions, LoopbackHub};
use regia_net::voice::*;
use std::sync::Mutex;

const QUALITY: VoiceQuality = VoiceQuality::Standard;

fn voice_settings() -> VoiceChatSettings {
    VoiceChatSettings {
        deterministic: true,
        quality: QUALITY,
        ..VoiceChatSettings::default()
    }
}

fn setup(world: &mut World) {
    register_voice_messages(world);
    world.insert_resource(VoiceRelay::new(VoiceRelaySettings::default(), &voice_settings()));
}

fn add_client(hub: &mut LoopbackHub) -> usize {
    let index = hub.add_client();
    let client = &mut hub.clients[index];
    let user = VoiceChatUserSettings {
        consent: true,
        ..VoiceChatUserSettings::default()
    };
    let mut session = VoiceChatSession::new(voice_settings(), user).expect("session");
    session.set_codec_provider(opus_codecs()).expect("provider");
    session
        .set_channel(4, Some(true), Some(true), None)
        .expect("join");
    client
        .world
        .insert_resource(VoiceChatResource(Mutex::new(session)));
    client.gameplay.add_systems(voice_client_system);
    index
}

fn with<R>(world: &World, f: impl FnOnce(&mut VoiceChatSession) -> R) -> R {
    let mut session = world.resource::<VoiceChatResource>().0.lock().expect("lock");
    f(&mut session)
}

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

fn rms_db(samples: &[f32]) -> f32 {
    let mean = samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len().max(1) as f64;
    (10.0 * mean.max(1e-12).log10()) as f32
}

/// A 44.1 kHz stereo microphone on client 0 talks to client 1; returns what client 1 played.
fn talk(down: LinkConditions, frames: usize) -> (Vec<f32>, SpeakerStats) {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 11, setup);
    hub.server.gameplay.add_systems(voice_server_system);
    let (talker, listener) = (add_client(&mut hub), add_client(&mut hub));
    hub.down = down;
    for _ in 0..8 {
        hub.run(1);
    }
    let slot = hub.clients[talker]
        .world
        .resource::<NetVoiceLink>()
        .slot()
        .expect("seated");
    let mic = FakeMicrophone::new(44_100, 2);
    mic.set_tone(Some(TestSource::sine(440.0, -12.0)));
    with(&hub.clients[talker].world, |s| {
        s.set_microphone(Box::new(mic.clone()));
        s.set_mode(VoiceMode::Open);
        s.enable_microphone().expect("enable");
    });
    assert_eq!(mic.opens(), 1);
    let mut played = Vec::new();
    for _ in 0..frames {
        with(&hub.clients[talker].world, |s| s.tick(false));
        hub.run(1);
        with(&hub.clients[listener].world, |s| {
            s.tick(false);
            for (id, rate, samples) in s.playout() {
                assert_eq!((*id, *rate), (slot, QUALITY.sample_rate()));
                played.extend_from_slice(samples);
            }
        });
    }
    let stats = with(&hub.clients[listener].world, |s| {
        assert_eq!(s.stats().bad_packets, 0);
        s.speaker(slot).expect("speaker").stats()
    });
    (played, stats)
}

#[test]
fn a_fake_microphone_is_heard_through_opus_and_the_relay() {
    let (played, stats) = talk(LinkConditions::default(), 150);
    assert_eq!(stats.decode_errors, 0);
    assert_eq!(stats.jitter.concealed, 0, "{stats:?}");
    let settled = &played[played.len() / 2..];
    let rate = QUALITY.sample_rate();
    assert!(power_at(settled, rate, 440.0) > power_at(settled, rate, 880.0) * 100.0);
    assert!(power_at(settled, rate, 440.0) > power_at(settled, rate, 220.0) * 100.0);
    // A -12 dBFS peak sine is -15 dB RMS.
    assert!((rms_db(settled) + 15.0).abs() < 3.0, "{}", rms_db(settled));
}

#[test]
fn a_lossy_downlink_stays_clean_and_keeps_the_tone() {
    let down = LinkConditions {
        loss: 0.1,
        reorder: 0.05,
        delay_ticks: 1,
        ..LinkConditions::default()
    };
    let (played, stats) = talk(down, 400);
    assert!(stats.jitter.recovered + stats.jitter.concealed > 0, "{stats:?}");
    assert_eq!(stats.decode_errors, 0);
    assert!(played.iter().all(|s| s.is_finite()));
    let frame = QUALITY.frame_samples();
    let peaks: Vec<f32> = played
        .chunks_exact(frame)
        .map(|f| f.iter().fold(0.0f32, |m, s| m.max(s.abs())))
        .collect();
    let loudest = peaks.iter().copied().fold(0.0, f32::max);
    let at = peaks.iter().position(|p| *p == loudest);
    let around: Vec<f32> = at.map_or(Vec::new(), |i| {
        peaks[i.saturating_sub(3)..(i + 3).min(peaks.len())].to_vec()
    });
    // Full scale is what the library's own concealment gives. A pure tone is the worst case for a
    // decoder with a gap in its memory: this -12 dBFS tone at 10 % loss peaks near 0.7 (ADR-1911).
    assert!(
        loudest < 0.8,
        "never a blast: {loudest} at {at:?}, around {around:?}, {stats:?}"
    );
    let settled = &played[played.len() / 2..];
    let rate = QUALITY.sample_rate();
    assert!(power_at(settled, rate, 440.0) > power_at(settled, rate, 880.0) * 10.0);
}
