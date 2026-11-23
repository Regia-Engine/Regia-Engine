// SPDX-License-Identifier: MPL-2.0
//! The v0.5 security review of voice (ADR-5870 § 7): sender slots come from the server, which on
//! a listen server is another player's machine, so the number of remote speakers and of bound
//! sender slots a client keeps is bounded.

use regia_audio::voice_chat::*;

fn settings() -> VoiceChatSettings {
    VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        ..VoiceChatSettings::default()
    }
}

fn listener() -> VoiceChatSession {
    let mut session = VoiceChatSession::new(settings(), VoiceChatUserSettings::default()).expect("session");
    session.set_channel(1, Some(true), None, None).expect("join");
    session
}

/// One real packet on channel 1, from a transmitting session.
fn template() -> Vec<u8> {
    let mic = FakeMicrophone::new(16_000, 1);
    let user = VoiceChatUserSettings {
        consent: true,
        ..VoiceChatUserSettings::default()
    };
    let mut tx = VoiceChatSession::new(settings(), user).expect("session");
    tx.set_microphone(Box::new(mic.clone()));
    tx.set_mode(VoiceMode::Open);
    tx.set_channel(1, Some(true), Some(true), None).expect("join");
    tx.enable_microphone().expect("enable");
    tx.set_local_sender(1);
    for frame in 0..50 {
        let pcm: Vec<f32> = (0..320)
            .map(|i| ((frame * 320 + i) as f32 * 0.05).sin() * 0.3)
            .collect();
        mic.push(&pcm);
        tx.tick(false);
        if let Some(packet) = tx.take_outgoing().into_iter().next() {
            return packet;
        }
    }
    panic!("the transmitter sent nothing");
}

fn from_sender(template: &[u8], sender: u16) -> Vec<u8> {
    let parsed = VoicePacket::parse(template).expect("parses");
    let mut out = Vec::new();
    VoicePacket { sender, ..parsed }.write(&mut out);
    out
}

/// A server inventing a new sender slot per packet made the client build a decoder and a jitter
/// buffer for each, rendered every frame, with no limit.
#[test]
fn remote_speakers_are_bounded() {
    let template = template();
    let mut rx = listener();
    let extra = 50;
    for sender in 0..(MAX_REMOTE_SPEAKERS + extra) as u16 {
        rx.receive_packet(&from_sender(&template, sender + 10));
    }
    assert_eq!(rx.speaker_stats().len(), MAX_REMOTE_SPEAKERS);
    assert_eq!(rx.stats().overflow_drops, extra as u64);
    // A known speaker still gets through.
    let before = rx.stats().packets_received;
    rx.receive_packet(&from_sender(&template, 10));
    assert_eq!(rx.stats().packets_received, before + 1);
}

/// Roster notices bind slots to accounts: past the cap a new slot is ignored, and a binding still
/// applies the local block list to its own speaker.
#[test]
fn sender_bindings_are_bounded_and_still_apply_blocks() {
    let template = template();
    let mut rx = listener();
    rx.block_account("griefer", "", true);
    rx.bind_sender(7, "griefer");
    assert!(rx.is_sender_silenced(7));
    rx.receive_packet(&from_sender(&template, 7));
    assert_eq!(rx.stats().muted_drops, 1);
    for slot in 0..MAX_BOUND_SENDERS as u16 * 2 {
        rx.bind_sender(1000 + slot, &format!("player{slot}"));
    }
    // Past the cap a new binding is ignored, even to a blocked account.
    rx.bind_sender(60_000, "griefer");
    assert!(!rx.is_sender_silenced(60_000));
    // A slot already bound can still change account.
    rx.bind_sender(7, "friend");
    assert!(!rx.is_sender_silenced(7));
}
