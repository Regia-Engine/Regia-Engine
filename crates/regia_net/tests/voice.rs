// SPDX-License-Identifier: MPL-2.0
//! Voice chat over the net runtime, in one process at fixed steps (specs/59 § 2.10, acceptance 9, 11).
//! A test source feeds real sessions; `LoopbackHub` carries the packets.
#![cfg(feature = "voice")]

use regia_audio::voice_chat::*;
use regia_ecs::prelude::*;
use regia_net::model::{NetMode, PeerId};
use regia_net::testing::{LinkConditions, LoopbackHub};
use regia_net::voice::*;
use std::sync::Mutex;

fn voice_settings() -> VoiceChatSettings {
    VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        ..VoiceChatSettings::default()
    }
}

fn packet_from(sender: u16, channel: u16, seq: u16) -> Vec<u8> {
    packet_len(sender, channel, seq, 1)
}

fn packet_len(sender: u16, channel: u16, seq: u16, len: usize) -> Vec<u8> {
    let payload = vec![1u8; len];
    let packet = VoicePacket {
        flags: VoiceFlags::default(),
        sender,
        channel,
        seq,
        timestamp: u32::from(seq) * FRAME_TIMESTAMP_STEP,
        quality: VoiceQuality::Narrow,
        codec: VoiceCodecId::Pcm,
        payload: &payload,
    };
    let mut out = Vec::new();
    packet.write(&mut out);
    out
}

fn heard_by(relay: &mut VoiceRelay, listener: PeerId) -> Vec<u16> {
    delivered(relay)
        .into_iter()
        .filter(|(peer, _)| *peer == listener)
        .flat_map(|(_, packets)| packets)
        .map(|bytes| VoicePacket::parse(&bytes).expect("packet").sender)
        .collect()
}

fn relay() -> VoiceRelay {
    VoiceRelay::new(VoiceRelaySettings::default(), &voice_settings())
}

fn joined(relay: &mut VoiceRelay, peer: u32, channel: u16) -> u16 {
    let slot = relay.add_peer(PeerId(peer)).expect("slot");
    relay.control(PeerId(peer), &VoiceControl::Join(channel));
    slot
}

fn delivered(relay: &mut VoiceRelay) -> Vec<(PeerId, Vec<Vec<u8>>)> {
    relay.take_notices();
    relay.take_outgoing()
}

#[test]
fn the_relay_forwards_to_channel_members_only_and_stamps_the_sender() {
    let mut relay = relay();
    let a = joined(&mut relay, 1, 4);
    let b = joined(&mut relay, 2, 4);
    joined(&mut relay, 3, 1);
    assert_ne!(a, b);
    // A client that lies about its sender id.
    relay.route(PeerId(1), &packet_from(999, 4, 0), 0);
    let out = delivered(&mut relay);
    assert_eq!(out.len(), 1, "the sender and the other channel get nothing");
    assert_eq!(out[0].0, PeerId(2));
    let packet = VoicePacket::parse(&out[0].1[0]).expect("parses");
    assert_eq!(packet.sender, a, "the server overwrites the sender");
    assert_eq!(relay.stats().forwarded, 1);
}

#[test]
fn a_sender_outside_the_channel_and_garbage_are_counted_not_forwarded() {
    let mut relay = relay();
    joined(&mut relay, 1, 1);
    joined(&mut relay, 2, 4);
    relay.route(PeerId(1), &packet_from(0, 4, 0), 0);
    relay.route(PeerId(1), &[1, 2, 3], 0);
    relay.route(PeerId(9), &packet_from(0, 4, 0), 0);
    assert_eq!(relay.stats().not_in_channel, 1);
    assert_eq!(relay.stats().bad_packets, 1);
    assert!(delivered(&mut relay).is_empty());
}

#[test]
fn joining_an_unknown_channel_does_nothing() {
    let mut relay = relay();
    joined(&mut relay, 1, 77);
    assert!(relay.members_of(77).is_empty());
}

#[test]
fn proximity_uses_radius_plus_slack_and_the_relevancy_hook() {
    let mut relay = relay();
    joined(&mut relay, 1, 2);
    joined(&mut relay, 2, 2);
    relay.set_position(PeerId(1), [0.0; 3]);
    relay.set_position(PeerId(2), [29.0, 0.0, 0.0]);
    relay.route(PeerId(1), &packet_from(0, 2, 0), 0);
    assert_eq!(delivered(&mut relay).len(), 1, "25 m radius plus 5 m slack");
    relay.set_position(PeerId(2), [31.0, 0.0, 0.0]);
    relay.route(PeerId(1), &packet_from(0, 2, 1), 20);
    assert_eq!(
        delivered(&mut relay).len(),
        1,
        "the in-range decision is reused for 100 ms"
    );
    relay.route(PeerId(1), &packet_from(0, 2, 2), 100);
    assert!(
        delivered(&mut relay).is_empty(),
        "after 100 ms the listener is past radius plus slack"
    );
    relay.set_position(PeerId(2), [1.0, 0.0, 0.0]);
    relay.set_excluded(PeerId(2), true);
    relay.route(PeerId(1), &packet_from(0, 2, 3), 200);
    assert!(
        delivered(&mut relay).is_empty(),
        "exclusion is applied on the packet, not from the distance cache"
    );
}

#[test]
fn the_listener_cap_holds_its_set_for_200_ms() {
    let mut relay = VoiceRelay::new(
        VoiceRelaySettings {
            rate_per_second: 1_000.0,
            burst: 100.0,
            max_mixed_speakers: 1,
        },
        &voice_settings(),
    );
    let listener_slot = joined(&mut relay, 10, 4);
    let quiet_a = joined(&mut relay, 1, 4);
    let quiet_b = joined(&mut relay, 2, 4);
    let quiet_c = joined(&mut relay, 3, 4);
    let loud = joined(&mut relay, 4, 4);
    let listener = PeerId(10);
    let send = |relay: &mut VoiceRelay, peer: u32, slot: u16, seq: u16, len: usize, now: u64| {
        relay.route(PeerId(peer), &packet_len(slot, 4, seq, len), now);
    };
    send(&mut relay, 1, quiet_a, 0, 40, 0);
    send(&mut relay, 2, quiet_b, 0, 40, 0);
    send(&mut relay, 3, quiet_c, 0, 40, 0);
    send(&mut relay, 4, loud, 0, 200, 0);
    let _ = delivered(&mut relay);
    send(&mut relay, 1, quiet_a, 1, 40, 50);
    send(&mut relay, 2, quiet_b, 1, 40, 50);
    send(&mut relay, 3, quiet_c, 1, 40, 50);
    send(&mut relay, 4, loud, 1, 39, 50);
    let held = heard_by(&mut relay, listener);
    assert!(
        held.contains(&loud),
        "a quieter next packet stays for 200 ms: {held:?}"
    );
    assert!(held.contains(&quiet_a) && held.contains(&quiet_b), "{held:?}");
    assert!(!held.contains(&quiet_c), "the dropped sender stays out: {held:?}");
    assert_ne!(listener_slot, loud);
    // Still inside the 200 ms hold: the new sizes are stored, the set is not.
    send(&mut relay, 1, quiet_a, 2, 40, 199);
    send(&mut relay, 2, quiet_b, 2, 40, 199);
    send(&mut relay, 3, quiet_c, 2, 100, 199);
    send(&mut relay, 4, loud, 2, 39, 199);
    let still = heard_by(&mut relay, listener);
    assert!(still.contains(&loud) && !still.contains(&quiet_c), "{still:?}");
    // The first packet after the hold re-ranks from the sizes stored at t=199.
    send(&mut relay, 3, quiet_c, 3, 100, 250);
    send(&mut relay, 4, loud, 3, 39, 250);
    let ranked = heard_by(&mut relay, listener);
    assert!(
        ranked.contains(&quiet_c),
        "after 200 ms a clearly louder sender enters: {ranked:?}"
    );
    assert!(
        !ranked.contains(&loud),
        "the stay-in margin does not keep a sender 61 units quieter: {ranked:?}"
    );
}

#[test]
fn a_block_is_mutual_and_follows_the_account() {
    let mut relay = relay();
    joined(&mut relay, 1, 4);
    joined(&mut relay, 2, 4);
    relay.set_account(PeerId(1), "acct-a");
    relay.set_account(PeerId(2), "acct-b");
    let block = |blocked| VoiceControl::Block {
        account: "acct-b".into(),
        blocked,
    };
    relay.control(PeerId(1), &block(true));
    relay.route(PeerId(1), &packet_from(0, 4, 0), 0);
    relay.route(PeerId(2), &packet_from(0, 4, 0), 20);
    assert!(delivered(&mut relay).is_empty(), "neither way");
    relay.control(PeerId(1), &block(false));
    relay.route(PeerId(1), &packet_from(0, 4, 1), 40);
    assert_eq!(delivered(&mut relay).len(), 1);
}

#[test]
fn a_mute_hint_stops_forwarding_to_that_listener_only() {
    let mut relay = relay();
    let a = joined(&mut relay, 1, 4);
    joined(&mut relay, 2, 4);
    joined(&mut relay, 3, 4);
    relay.control(
        PeerId(2),
        &VoiceControl::MuteHint {
            sender: a,
            muted: true,
        },
    );
    relay.route(PeerId(1), &packet_from(0, 4, 0), 0);
    let out = delivered(&mut relay);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, PeerId(3));
}

#[test]
fn the_rate_limit_allows_speech_and_cuts_a_flood() {
    let mut relay = relay();
    joined(&mut relay, 1, 4);
    joined(&mut relay, 2, 4);
    // Two seconds of real speech: 50 packets a second.
    for seq in 0..100u16 {
        relay.route(PeerId(1), &packet_from(0, 4, seq), u64::from(seq) * 20);
    }
    assert_eq!(relay.stats().rate_limited, 0);
    relay.take_outgoing();
    // A flood in one instant: the burst passes, then nothing.
    let now = 10_000;
    for seq in 0..100u16 {
        relay.route(PeerId(1), &packet_from(0, 4, seq), now);
    }
    assert_eq!(relay.stats().rate_limited, 90);
}

#[test]
fn a_server_mute_drops_until_lifted_and_notifies() {
    let mut relay = relay();
    joined(&mut relay, 1, 4);
    joined(&mut relay, 2, 4);
    relay.take_notices();
    relay.server_mute(PeerId(1), u32::MAX);
    assert!(
        relay
            .take_notices()
            .iter()
            .any(|n| matches!(n, RelayNotice::ServerMuted { to: PeerId(1), .. }))
    );
    relay.route(PeerId(1), &packet_from(0, 4, 0), 0);
    assert_eq!(relay.stats().server_muted, 1);
    relay.server_mute(PeerId(1), 0);
    relay.route(PeerId(1), &packet_from(0, 4, 1), 20);
    assert_eq!(relay.take_outgoing().len(), 1);
}

struct OnlyTeamOne;
impl VoiceChatModeration for OnlyTeamOne {
    fn allow_join(&mut self, peer: PeerId, channel: u16) -> bool {
        channel != 1 || peer == PeerId(1)
    }
    fn allow_transmit(&mut self, peer: PeerId, _channel: u16) -> bool {
        peer != PeerId(2)
    }
}

#[test]
fn moderation_hooks_decide_joins_and_transmits() {
    let mut relay = relay();
    relay.set_moderation(OnlyTeamOne);
    joined(&mut relay, 1, 1);
    joined(&mut relay, 2, 1);
    assert_eq!(relay.members_of(1).len(), 1);
    assert_eq!(relay.stats().joins_refused, 1);
    joined(&mut relay, 2, 4);
    joined(&mut relay, 3, 4);
    relay.route(PeerId(2), &packet_from(0, 4, 0), 0);
    assert_eq!(relay.stats().server_muted, 1);
}

#[test]
fn the_listener_cap_keeps_the_highest_priority_sender() {
    let settings = VoiceRelaySettings {
        max_mixed_speakers: 1,
        ..VoiceRelaySettings::default()
    };
    let mut relay = VoiceRelay::new(settings, &voice_settings());
    // Slot 0 listens to everything; slots 1..=4 talk. Team (1) outranks all (4).
    relay.add_peer(PeerId(100)).expect("slot");
    relay.control(PeerId(100), &VoiceControl::Join(1));
    relay.control(PeerId(100), &VoiceControl::Join(4));
    for peer in 1..=3 {
        joined(&mut relay, peer, 4);
    }
    joined(&mut relay, 4, 1);
    for round in 0..20u16 {
        for peer in 1..=3u32 {
            relay.route(PeerId(peer), &packet_from(0, 4, round), u64::from(round) * 20);
        }
        relay.route(PeerId(4), &packet_from(0, 1, round), u64::from(round) * 20);
    }
    assert!(relay.stats().listener_cap_drops > 0);
    let to_listener: Vec<u16> = relay
        .take_outgoing()
        .into_iter()
        .filter(|(p, _)| *p == PeerId(100))
        .flat_map(|(_, packets)| packets)
        .map(|p| VoicePacket::parse(&p).expect("parses").sender)
        .collect();
    let last_round = &to_listener[to_listener.len() - 3..];
    let team = relay.slot(PeerId(4)).expect("slot");
    assert!(last_round.contains(&team), "{last_round:?}");
    let distinct: std::collections::BTreeSet<u16> = to_listener.iter().copied().collect();
    assert!(
        distinct.len() <= 4,
        "at most one sender loses its slot to the team: {distinct:?}"
    );
}

#[test]
fn a_freed_slot_is_reused_and_announced() {
    let mut relay = relay();
    let a = joined(&mut relay, 1, 4);
    joined(&mut relay, 2, 4);
    relay.take_notices();
    relay.remove_peer(PeerId(1));
    assert!(relay.take_notices().contains(&RelayNotice::Left {
        to: PeerId(2),
        slot: a
    }));
    assert_eq!(relay.add_peer(PeerId(3)), Some(a));
    assert_eq!(
        relay.members_of(4).len(),
        1,
        "a new slot holder starts in no channel"
    );
}

#[test]
fn uplink_loss_is_reported_to_the_sender() {
    let mut relay = relay();
    joined(&mut relay, 1, 4);
    relay.take_notices();
    let mut seq = 0u16;
    let mut now = 0;
    for i in 0..120u32 {
        relay.route(PeerId(1), &packet_from(0, 4, seq), now);
        // One packet in five is lost on the way.
        seq = seq.wrapping_add(if i % 4 == 3 { 2 } else { 1 });
        now += 25;
    }
    let reported = relay.take_notices().iter().find_map(|n| match n {
        RelayNotice::Link { loss_pct, .. } => Some(*loss_pct),
        _ => None,
    });
    let loss = reported.expect("a report");
    assert!((15..=25).contains(&loss), "{loss}");
}

#[test]
fn two_frame_packets_are_not_counted_as_loss() {
    let mut relay = relay();
    joined(&mut relay, 1, 4);
    relay.take_notices();
    let mut payload = Vec::new();
    write_multi(&[1], &[1], false, &mut payload);
    for i in 0..120u16 {
        let packet = VoicePacket {
            flags: VoiceFlags::default().with(VoiceFlags::MULTI, true),
            sender: 0,
            channel: 4,
            seq: i * 2,
            timestamp: u32::from(i) * 2 * FRAME_TIMESTAMP_STEP,
            quality: VoiceQuality::Narrow,
            codec: VoiceCodecId::Pcm,
            payload: &payload,
        };
        let mut wire = Vec::new();
        packet.write(&mut wire);
        relay.route(PeerId(1), &wire, u64::from(i) * 40);
    }
    let reported = relay.take_notices().iter().find_map(|n| match n {
        RelayNotice::Link { loss_pct, .. } => Some(*loss_pct),
        _ => None,
    });
    assert_eq!(reported, Some(0));
}

// ----- end to end over the hub -----------------------------------------------------------------

fn setup(world: &mut World) {
    register_voice_messages(world);
    world.insert_resource(VoiceRelay::new(VoiceRelaySettings::default(), &voice_settings()));
}

fn add_session(world: &mut World) {
    let session = VoiceChatSession::new(voice_settings(), VoiceChatUserSettings::default()).expect("session");
    world.insert_resource(VoiceChatResource(Mutex::new(session)));
}

fn with<R>(world: &World, f: impl FnOnce(&mut VoiceChatSession) -> R) -> R {
    let mut session = world.resource::<VoiceChatResource>().0.lock().expect("lock");
    f(&mut session)
}

/// A server and `clients` clients, each client with a session in channel 4.
fn hub(mode: NetMode, clients: usize) -> LoopbackHub {
    let mut hub = LoopbackHub::new(mode, 7, setup);
    hub.server.gameplay.add_systems(voice_server_system);
    for _ in 0..clients {
        let index = hub.add_client();
        let client = &mut hub.clients[index];
        add_session(&mut client.world);
        client.gameplay.add_systems(voice_client_system);
        with(&client.world, |s| {
            s.set_channel(4, Some(true), Some(true), None).expect("join");
        });
    }
    hub
}

/// One 20 ms voice frame on every session, then `ticks` net ticks.
fn voice_frame(hub: &mut LoopbackHub, ticks: usize) {
    for client in &hub.clients {
        with(&client.world, |s| s.tick(false));
    }
    if hub.server.world.contains_resource::<VoiceChatResource>() {
        with(&hub.server.world, |s| s.tick(false));
    }
    hub.run(ticks);
}

fn start_talking(world: &World, hz: f64) {
    with(world, |s| {
        s.set_test_source(Some(TestSource::sine(hz, -12.0)));
        s.set_mode(VoiceMode::Open);
    });
}

fn slot(hub: &LoopbackHub, client: usize) -> u16 {
    hub.clients[client]
        .world
        .resource::<NetVoiceLink>()
        .slot()
        .expect("seated")
}

#[test]
fn two_clients_hear_each_other_through_a_dedicated_server() {
    let mut hub = hub(NetMode::DedicatedServer, 2);
    voice_frame(&mut hub, 4);
    let (a, b) = (slot(&hub, 0), slot(&hub, 1));
    assert_ne!(a, b);
    assert_eq!(
        hub.server.world.resource::<VoiceRelay>().members_of(4),
        vec![a, b]
    );
    start_talking(&hub.clients[0].world, 440.0);
    for _ in 0..30 {
        voice_frame(&mut hub, 1);
    }
    with(&hub.clients[1].world, |s| {
        let speaker = s.speaker(a).expect("client 1 hears slot a");
        assert_eq!(speaker.state(), SpeakerState::Speaking);
        assert!(s.stats().packets_received > 20, "{:?}", s.stats());
        assert_eq!(s.stats().bad_packets, 0);
    });
    with(&hub.clients[0].world, |s| {
        assert!(
            s.speaker(a).is_none(),
            "a player never receives their own packets"
        );
    });
    assert!(hub.server.world.resource::<VoiceRelay>().stats().forwarded > 20);
}

#[test]
fn a_listen_servers_player_talks_and_listens() {
    let mut hub = hub(NetMode::ListenServer, 1);
    add_session(&mut hub.server.world);
    with(&hub.server.world, |s| {
        s.set_channel(4, Some(true), Some(true), None).expect("join");
    });
    voice_frame(&mut hub, 4);
    let client_slot = slot(&hub, 0);
    let host_slot = hub
        .server
        .world
        .resource::<NetVoiceLink>()
        .slot()
        .expect("seated");
    assert_ne!(client_slot, host_slot);
    start_talking(&hub.clients[0].world, 440.0);
    start_talking(&hub.server.world, 330.0);
    for _ in 0..30 {
        voice_frame(&mut hub, 1);
    }
    with(&hub.server.world, |s| {
        assert_eq!(
            s.speaker(client_slot).expect("host hears client").state(),
            SpeakerState::Speaking
        );
    });
    with(&hub.clients[0].world, |s| {
        assert_eq!(
            s.speaker(host_slot).expect("client hears host").state(),
            SpeakerState::Speaking
        );
    });
}

#[test]
fn voice_survives_a_lossy_link_and_the_receiver_conceals() {
    let mut hub = hub(NetMode::DedicatedServer, 2);
    hub.down = LinkConditions {
        loss: 0.15,
        reorder: 0.1,
        duplicate: 0.05,
        delay_ticks: 2,
        ..LinkConditions::default()
    };
    hub.up = LinkConditions {
        loss: 0.05,
        delay_ticks: 1,
        ..LinkConditions::default()
    };
    voice_frame(&mut hub, 20);
    let a = slot(&hub, 0);
    start_talking(&hub.clients[0].world, 440.0);
    for _ in 0..300 {
        voice_frame(&mut hub, 1);
    }
    with(&hub.clients[1].world, |s| {
        let (_, stats) = s
            .speaker_stats()
            .into_iter()
            .find(|(id, _)| *id == a)
            .expect("speaker");
        assert!(stats.jitter.played > 100, "{stats:?}");
        assert!(stats.jitter.concealed + stats.jitter.recovered > 0, "{stats:?}");
        assert_eq!(stats.decode_errors, 0);
    });
}

#[test]
fn a_mute_hint_saves_the_downlink() {
    let mut hub = hub(NetMode::DedicatedServer, 2);
    voice_frame(&mut hub, 4);
    let a = slot(&hub, 0);
    start_talking(&hub.clients[0].world, 440.0);
    for _ in 0..10 {
        voice_frame(&mut hub, 1);
    }
    assert!(hub.server.world.resource::<VoiceRelay>().stats().forwarded > 0);
    with(&hub.clients[1].world, |s| {
        s.mute_sender(a, true);
    });
    for _ in 0..4 {
        voice_frame(&mut hub, 1);
    }
    let settled = hub.server.world.resource::<VoiceRelay>().stats().forwarded;
    for _ in 0..20 {
        voice_frame(&mut hub, 1);
    }
    assert_eq!(
        hub.server.world.resource::<VoiceRelay>().stats().forwarded,
        settled
    );
}

#[test]
fn the_roster_binds_accounts_so_a_persisted_mute_follows_the_player() {
    let mut hub = hub(NetMode::DedicatedServer, 2);
    voice_frame(&mut hub, 4);
    let a = slot(&hub, 0);
    with(&hub.clients[1].world, |s| s.mute_account("acct-a", "", true));
    hub.server
        .world
        .resource_mut::<VoiceRelay>()
        .set_account(PeerId(1), "acct-a");
    voice_frame(&mut hub, 4);
    with(&hub.clients[1].world, |s| assert!(s.is_sender_silenced(a)));
}

#[test]
fn a_quality_cap_moves_senders_at_their_next_pause() {
    let mut hub = hub(NetMode::DedicatedServer, 1);
    with(&hub.clients[0].world, |s| s.set_quality(VoiceQuality::High));
    voice_frame(&mut hub, 4);
    with(&hub.clients[0].world, |s| {
        assert_eq!(s.quality(), VoiceQuality::High)
    });
    hub.server
        .world
        .resource_mut::<VoiceRelay>()
        .set_quality_cap(Some(VoiceQuality::Narrow));
    voice_frame(&mut hub, 4);
    voice_frame(&mut hub, 1);
    with(&hub.clients[0].world, |s| {
        assert_eq!(s.quality(), VoiceQuality::Narrow)
    });
}

#[test]
fn a_departed_players_speaker_is_dropped() {
    let mut hub = hub(NetMode::DedicatedServer, 2);
    voice_frame(&mut hub, 4);
    let a = slot(&hub, 0);
    start_talking(&hub.clients[0].world, 440.0);
    for _ in 0..10 {
        voice_frame(&mut hub, 1);
    }
    with(&hub.clients[1].world, |s| assert!(s.speaker(a).is_some()));
    regia_net::runtime::server_remove_peer(&mut hub.server.world, PeerId(1));
    hub.clients[0].connected = false;
    for _ in 0..5 {
        with(&hub.clients[1].world, |s| s.tick(false));
        hub.run(1);
    }
    with(&hub.clients[1].world, |s| assert!(s.speaker(a).is_none()));
}

#[test]
fn a_join_before_the_welcome_is_repeated_after_it() {
    // The join happened in `hub()` before any tick: the welcome replays it.
    let mut hub = hub(NetMode::DedicatedServer, 1);
    voice_frame(&mut hub, 6);
    let seated = slot(&hub, 0);
    assert_eq!(
        hub.server.world.resource::<VoiceRelay>().members_of(4),
        vec![seated]
    );
}

#[test]
fn the_link_is_a_transport() {
    let mut link = NetVoiceLink::default();
    link.send(&[1, 2, 3]);
    assert!(link.poll().is_none());
    assert_eq!(link.link_quality(), None);
}

/// Spec 59 § 5: a server mute reaches the muted client's session as `server_muted`: an
/// `Error(ServerMuted)` event for the game (and a record `voice_chat.status` reports).
#[test]
fn a_server_mute_reaches_the_muted_clients_session_as_server_muted() {
    let mut hub = hub(NetMode::DedicatedServer, 2);
    voice_frame(&mut hub, 4);
    let muted = slot(&hub, 0);
    with(&hub.clients[0].world, |s| drop(s.take_events()));
    with(&hub.clients[1].world, |s| drop(s.take_events()));
    let peer = hub
        .server
        .world
        .resource::<VoiceRelay>()
        .peers()
        .find(|(_, slot)| *slot == muted)
        .map(|(peer, _)| peer)
        .expect("seated peer");
    hub.server
        .world
        .resource_mut::<VoiceRelay>()
        .server_mute(peer, u32::MAX);
    voice_frame(&mut hub, 3);
    let server_muted = VoiceChatEvent::Error(VoiceChatError::ServerMuted);
    with(&hub.clients[0].world, |s| {
        assert!(
            s.take_events().contains(&server_muted),
            "the muted client is told"
        );
    });
    with(&hub.clients[1].world, |s| {
        assert!(!s.take_events().contains(&server_muted), "only the muted client");
    });
}
