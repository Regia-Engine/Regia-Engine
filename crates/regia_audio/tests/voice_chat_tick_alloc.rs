// SPDX-License-Identifier: MPL-2.0
//! Steady voice-chat playout of scripted speakers allocates nothing per voice frame.
//!
//! Jitter wants about 60 ms (three 20 ms frames) and speaker ranking runs every ten frames, so
//! one warmup tick is not steady state. Forty ticks size the pools; the next hundred are measured.
//! The session joins channel 1 first: packets on an unjoined channel never reach a speaker.
//! With the output tap on, played frames wait for the consumer's acknowledge (ADR-5589).

#![allow(unsafe_code)]

include!("support/alloc_count.rs");

use regia_audio::voice_chat::{TestSource, VoiceChatSession, VoiceChatSettings, VoiceChatUserSettings};

fn session() -> VoiceChatSession {
    match VoiceChatSession::new(VoiceChatSettings::default(), VoiceChatUserSettings::default()) {
        Ok(session) => session,
        Err(error) => panic!("session: {error}"),
    }
}

#[test]
fn steady_voice_frames_allocate_nothing() {
    let mut session = session();
    session.set_output_tap(true);
    match session.set_channel(1, Some(true), Some(true), None) {
        Ok(_) => {}
        Err(error) => panic!("join: {error}"),
    }
    session.set_fake_speaker(1, Some(TestSource::sine(180.0, -18.0)));
    session.set_fake_speaker(2, Some(TestSource::sine(240.0, -18.0)));
    session.set_fake_speaker(3, Some(TestSource::sine(320.0, -18.0)));
    // The output keeps every played frame until its consumer acknowledges them (the
    // voice-chat output system does so once per game update), so each tick here is followed by
    // the consumer's acknowledge, as in the app.
    for _ in 0..40 {
        session.tick(false);
        session.acknowledge_output();
    }
    let before = allocations();
    for _ in 0..100 {
        session.tick(false);
        session.acknowledge_output();
    }
    let allocs = allocations() - before;
    assert_eq!(allocs, 0, "100 steady voice frames allocated {allocs} times");

    let mut placed = Vec::new();
    session.place_speakers(&mut placed);
    assert_eq!(placed.len(), 3, "the three scripted speakers");
    let before = allocations();
    for _ in 0..60 {
        session.place_speakers(&mut placed);
    }
    let allocs = allocations() - before;
    assert_eq!(allocs, 0, "60 placement walks allocated {allocs} times");
    assert_eq!(placed.len(), 3);
}

#[test]
fn silent_channel_placement_allocates_nothing() {
    let session = session();
    let mut placed = Vec::new();
    session.place_speakers(&mut placed);
    assert_eq!(
        placed,
        vec![1, 2, 3, 4],
        "the default channel ids, with no name clone"
    );
    let before = allocations();
    for _ in 0..60 {
        session.place_speakers(&mut placed);
    }
    let allocs = allocations() - before;
    assert_eq!(allocs, 0, "60 silent placement walks allocated {allocs} times");
    assert_eq!(placed, vec![1, 2, 3, 4]);
}
