// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 10.8 (S7, H7, ADR-8351): the Steam transport on impaired links. The shared mock
//! network takes the soak harness's seeded impairment (latency, jitter, loss, bursts, reorder,
//! duplication, bandwidth, outages; `MockLink`), and lobby-launched Steam sessions run on
//! good-wifi, bad-wifi and flappy without the SDK: bad-wifi for five minutes with no disconnect,
//! flappy's 3 s outages without losing the player, and a suspend/resume over Steam on bad-wifi
//! that keeps the `PeerId`. Virtual time throughout (the sessions' manual clocks and
//! `MockSteamNet::advance_ms` move together); no wall clock, no sleeps.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_input::InputContexts;
use regia_net::lobby::LobbyServer;
use regia_net::session::lifecycle::{
    ClientSessionEvent, ClientSessionState, HostOptions, PeerState, ServerSessionEvent,
};
use regia_net::{DatagramSocket, Lobbies, NetSession, NetSessionEvents, NetSessionPlugin, PeerId};
use regia_platform::{AppLifecycle, LifecycleEvent};
use regia_steam::{
    MOCK_FLAP_OUTAGE_MS, MOCK_LINK_PROFILES, MockDirection, MockLink, MockSteam, MockSteamNet, SteamBackend,
    SteamId, SteamLobbyPlugin, SteamPlugin, SteamSettings, SteamSocket, steam_peer_addr,
};

const HOST: u64 = 76_561_190_000_000_310;
const GUEST: u64 = 76_561_190_000_000_320;
/// One frame of virtual time, milliseconds.
const FRAME_MS: u64 = 16;

fn user(net: &MockSteamNet, id: u64, name: &str) -> Arc<MockSteam> {
    let mock = MockSteam::shared();
    mock.join_net(net, SteamId(id), name);
    mock
}

fn app(mock: &Arc<MockSteam>) -> App {
    let mut app = App::new();
    app.insert_resource(SteamSettings::default())
        .insert_resource(AppLifecycle::default())
        .insert_resource(InputContexts::default());
    app.add_plugins(
        SteamPlugin::from_project_settings().with_backend(Arc::clone(mock) as Arc<dyn SteamBackend>),
    );
    let mut session = NetSession::default();
    session.set_manual_clock(0);
    app.insert_resource(session);
    app.add_plugins((NetSessionPlugin, SteamLobbyPlugin::default()));
    app.finish_plugins();
    app
}

/// Everything the two apps' sessions reported, accumulated over the run.
#[derive(Default)]
struct Log {
    host: Vec<ServerSessionEvent>,
    guest: Vec<ClientSessionEvent>,
}

/// A host and a guest app on one mock network, both users on `link`.
struct Pair {
    net: MockSteamNet,
    host: App,
    guest: App,
    log: Log,
}

impl Pair {
    fn new(link: MockLink, seed: u64) -> Self {
        let net = MockSteamNet::new();
        net.set_seed(seed);
        net.set_default_link(link);
        let ann = user(&net, HOST, "Ann");
        let bob = user(&net, GUEST, "Bob");
        Self {
            host: app(&ann),
            guest: app(&bob),
            net,
            log: Log::default(),
        }
    }

    /// One frame: the network and the apps' clocks move on, the apps update (the guest only when
    /// `guest_too`: a suspended phone does not run) and their events are logged.
    fn frame(&mut self, guest_too: bool) {
        self.net.advance_ms(FRAME_MS);
        tick(&mut self.host);
        let ev = self.host.world().resource::<NetSessionEvents>();
        self.log.host.extend(ev.server.iter().cloned());
        if guest_too {
            tick(&mut self.guest);
            let ev = self.guest.world().resource::<NetSessionEvents>();
            self.log.guest.extend(ev.client.iter().cloned());
        }
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.frame(true);
        }
    }

    /// Matchmakes, launches the host over Steam sockets and waits until the guest is in game.
    /// Returns the guest's `PeerId`.
    fn launch(&mut self) -> PeerId {
        self.frames(1);
        {
            let mut l = self.host.world_mut().resource_mut::<Lobbies>();
            let mut req = l.default_request();
            req.rounds.truncate(1);
            l.start_matchmaking(req).expect("matchmaking");
        }
        for _ in 0..3 {
            self.frame(false);
        }
        {
            let mut l = self.guest.world_mut().resource_mut::<Lobbies>();
            let req = l.default_request();
            l.start_matchmaking(req).expect("matchmaking");
        }
        // Lobby traffic is not on the impaired messages: it runs on the lobby hub.
        self.frames(3);
        let server = self
            .host
            .world_mut()
            .resource_scope(|world, mut lobbies: Mut<Lobbies>| {
                let mut session = world.resource_mut::<NetSession>();
                lobbies.launch_hosting(&mut session, HostOptions::default())
            })
            .expect("launch");
        assert_eq!(server, LobbyServer::Steam(HOST));
        // The join crosses the impaired link: up to 20 s of virtual time, until both ends agree
        // (the guest's `InGame` reaches the host one crossing later).
        for _ in 0..1_250 {
            self.frame(true);
            if self.guest_state() == ClientSessionState::InGame
                && self
                    .guest_peer()
                    .is_some_and(|p| self.host_peer_state(p) == Some(PeerState::InGame))
            {
                break;
            }
        }
        assert_eq!(
            self.guest_state(),
            ClientSessionState::InGame,
            "the guest joined over Steam"
        );
        self.guest_peer().expect("the guest was welcomed")
    }

    fn guest_state(&self) -> ClientSessionState {
        self.guest.world().resource::<NetSession>().state()
    }

    fn guest_peer(&self) -> Option<PeerId> {
        self.guest
            .world()
            .resource::<NetSession>()
            .client()
            .and_then(|c| c.peer_id())
    }

    fn host_peers(&self) -> Vec<PeerId> {
        self.host
            .world()
            .resource::<NetSession>()
            .server()
            .map(|s| s.peer_ids())
            .unwrap_or_default()
    }

    fn host_peer_state(&self, peer: PeerId) -> Option<PeerState> {
        self.host
            .world()
            .resource::<NetSession>()
            .server()
            .and_then(|s| s.peer_state(peer))
    }

    /// The host sends `n` as a reliable game message to `peer`.
    fn host_sends(&mut self, peer: PeerId, n: u32) {
        let mut session = self.host.world_mut().resource_mut::<NetSession>();
        if let Some(core) = session.server_mut() {
            core.send_game(peer, n.to_le_bytes().to_vec());
        }
    }

    /// The numbers the guest received in game messages, in arrival order.
    fn guest_received(&self) -> Vec<u32> {
        self.log
            .guest
            .iter()
            .filter_map(|e| match e {
                ClientSessionEvent::Game(b) if b.len() == 4 => {
                    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                }
                _ => None,
            })
            .collect()
    }

    /// Counts of the log's disconnect-like events: `(host drops or leaves, guest disconnects)`.
    fn disconnects(&self) -> (usize, usize) {
        let host = self
            .log
            .host
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ServerSessionEvent::PlayerDropped { .. } | ServerSessionEvent::PlayerLeft { .. }
                )
            })
            .count();
        let guest = self
            .log
            .guest
            .iter()
            .filter(|e| matches!(e, ClientSessionEvent::Disconnected { .. }))
            .count();
        (host, guest)
    }

    /// Runs `secs` seconds of a game: a reliable host message every 100 ms, and the guest checked
    /// in game, with the same `PeerId`, on every frame. Returns the messages sent.
    fn play(&mut self, peer: PeerId, secs: u64, first: u32) -> u32 {
        let frames = secs * 1_000 / FRAME_MS;
        let mut next = first;
        for f in 0..frames {
            if f % 6 == 0 {
                self.host_sends(peer, next);
                next += 1;
            }
            self.frame(true);
            let at = self.net.now_ms();
            assert_eq!(
                self.guest_state(),
                ClientSessionState::InGame,
                "the guest left the game at {at} ms"
            );
            assert_eq!(
                self.guest_peer(),
                Some(peer),
                "the guest's PeerId changed at {at} ms"
            );
            assert_eq!(
                self.host_peers(),
                vec![peer],
                "the host's peers changed at {at} ms"
            );
            assert_eq!(
                self.host_peer_state(peer),
                Some(PeerState::InGame),
                "the host lost the guest's slot at {at} ms"
            );
        }
        next
    }
}

/// One app frame of [`FRAME_MS`].
fn tick(app: &mut App) {
    app.world_mut()
        .resource_mut::<NetSession>()
        .advance_manual_clock(FRAME_MS);
    app.update();
}

/// Sends `count` numbered messages from A to B on one channel, one per `gap_ms`, and returns what
/// B received (numbers, in arrival order) once everything landed.
fn traffic(net: &MockSteamNet, count: u32, gap_ms: u64) -> Vec<u32> {
    let a = user(net, 11, "a");
    let b = user(net, 12, "b");
    let mut sa = SteamSocket::open(a.p2p().expect("p2p"), 2).expect("socket");
    let mut sb = SteamSocket::open(b.p2p().expect("p2p"), 2).expect("socket");
    let to = steam_peer_addr(SteamId(12), 2);
    let mut got = Vec::new();
    let mut buf = [0u8; 64];
    let mut drain = |sb: &mut SteamSocket, got: &mut Vec<u32>| {
        while let Some((n, _)) = sb.recv_from(&mut buf).expect("io") {
            assert_eq!(n, 4);
            got.push(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]));
        }
    };
    for i in 0..count {
        sa.send_to(&i.to_le_bytes(), to).expect("send");
        net.advance_ms(gap_ms);
        drain(&mut sb, &mut got);
    }
    // Let everything in flight land (reorder holds and the longest jitter are well under 1 s).
    net.advance_ms(1_000);
    drain(&mut sb, &mut got);
    got
}

#[test]
fn every_named_mock_link_resolves_and_clean_is_the_lossless_in_order_default() {
    for name in MOCK_LINK_PROFILES {
        assert!(MockLink::profile(name).is_some(), "{name}");
    }
    assert!(MockLink::profile("no-such-profile").is_none());
    assert!(MockLink::profile("clean").expect("clean").is_clean());
    let net = MockSteamNet::new();
    assert!(net.link(SteamId(11)).is_clean(), "users start on a clean link");
    // A clean network needs no time to pass: everything arrives at once and in order.
    let got = traffic(&net, 500, 0);
    assert_eq!(got, (0..500).collect::<Vec<_>>());
    assert_eq!(net.link_stats().impaired_sent, 0);
}

#[test]
fn bad_wifi_loses_reorders_and_duplicates_and_the_same_seed_replays_it() {
    let run = |seed: u64| {
        let net = MockSteamNet::new();
        net.set_seed(seed);
        net.set_default_link(MockLink::profile("bad-wifi").expect("bad-wifi"));
        let got = traffic(&net, 4_000, 10);
        (got, net.link_stats())
    };
    let (got, stats) = run(7);
    assert_eq!(stats.impaired_sent, 4_000);
    assert_eq!(stats.in_flight, 0, "everything landed");
    // Two bad-wifi directions (the sender's up, the receiver's down): ~1 % random and ~2 % burst
    // loss each.
    let lost = 4_000 - (got.len() as u64 - stats.duplicated);
    assert_eq!(lost, stats.drops(), "every missing message is a counted drop");
    assert!(stats.random_drops > 0 && stats.burst_drops > 0, "{stats:?}");
    assert!(
        (120..=480).contains(&lost),
        "bad-wifi lost {lost} of 4000: {stats:?}"
    );
    assert!(stats.reordered > 0 && stats.duplicated > 0, "{stats:?}");
    assert!(
        got.windows(2).any(|w| w[1] < w[0]),
        "a reorder hold lets later messages overtake"
    );
    assert!(got.windows(2).any(|w| w[1] == w[0]), "a duplicate arrives");
    assert_eq!(run(7), (got.clone(), stats), "the same seed replays the same run");
    assert_ne!(run(8).0, got, "another seed makes other decisions");
}

#[test]
fn a_flappy_outage_drops_everything_inside_its_window_and_nothing_outside() {
    let net = MockSteamNet::new();
    let a = user(&net, 21, "a");
    let b = user(&net, 22, "b");
    // Only the sender flaps; the receiver is clean.
    let link = MockLink {
        flaps: Some((1_000, 5_000, 2_000)),
        ..MockLink::clean()
    };
    net.set_link(SteamId(21), link);
    let mut sa = SteamSocket::open(a.p2p().expect("p2p"), 0).expect("socket");
    let mut sb = SteamSocket::open(b.p2p().expect("p2p"), 0).expect("socket");
    let mut buf = [0u8; 8];
    let mut arrived = Vec::new();
    for t in 0..12_000u64 {
        if t % 10 == 0 {
            sa.send_to(&(t as u32).to_le_bytes(), steam_peer_addr(SteamId(22), 0))
                .expect("send");
        }
        net.advance_ms(1);
        while let Some((_, _)) = sb.recv_from(&mut buf).expect("io") {
            arrived.push(u64::from(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])));
        }
    }
    // Outages at 1-3 s, 6-8 s, 11-13 s.
    let inside = |t: u64| (1_000..3_000).contains(&t) || (6_000..8_000).contains(&t) || t >= 11_000;
    let expected: Vec<u64> = (0..12_000).step_by(10).filter(|t| !inside(*t)).collect();
    assert_eq!(arrived, expected);
    assert_eq!(net.link_stats().outage_drops, 1_200 - expected.len() as u64);
    assert_eq!(
        sa.stats().send_errors,
        0,
        "an outage is silent loss, not a send error"
    );
}

#[test]
fn a_lobby_launched_steam_session_on_good_wifi_runs_a_minute_and_delivers_every_message() {
    let mut pair = Pair::new(MockLink::profile("good-wifi").expect("good-wifi"), 1);
    let peer = pair.launch();
    let sent = pair.play(peer, 60, 0);
    pair.frames(120);
    assert_eq!(
        pair.guest_received(),
        (0..sent).collect::<Vec<_>>(),
        "reliable, in order"
    );
    assert_eq!(pair.disconnects(), (0, 0));
    assert!(pair.net.link_stats().impaired_sent > 0);
}

#[test]
fn a_lobby_launched_steam_session_on_bad_wifi_runs_five_minutes_with_zero_disconnects() {
    let mut pair = Pair::new(MockLink::profile("bad-wifi").expect("bad-wifi"), 2);
    let peer = pair.launch();
    let sent = pair.play(peer, 300, 0);
    pair.frames(120);
    assert_eq!(
        pair.disconnects(),
        (0, 0),
        "no drop, leave or disconnect in five minutes"
    );
    assert_eq!(
        pair.guest_received(),
        (0..sent).collect::<Vec<_>>(),
        "every reliable message arrived once and in order"
    );
    let stats = pair.net.link_stats();
    eprintln!("bad-wifi 5 min: {sent} reliable messages, {stats:?}");
    assert!(
        stats.drops() > 100 && stats.reordered > 0 && stats.duplicated > 0,
        "the links really were impaired: {stats:?}"
    );
}

#[test]
fn a_flappy_steam_session_keeps_the_player_through_its_three_second_outages() {
    let mut pair = Pair::new(MockLink::profile("flappy").expect("flappy"), 3);
    let peer = pair.launch();
    // Two minutes: outages start at 15, 35, 55, 75, 95 and 115 s of network time.
    let sent = pair.play(peer, 120, 0);
    pair.frames(240);
    assert_eq!(pair.disconnects(), (0, 0), "a 3 s outage never drops the player");
    assert_eq!(
        pair.guest_received(),
        (0..sent).collect::<Vec<_>>(),
        "messages sent during an outage arrive after it"
    );
    let stats = pair.net.link_stats();
    assert!(stats.outage_drops > 0, "{stats:?}");
    // The outages were long enough to be seen: the host reported the guest interrupted and back.
    let interrupted = pair
        .log
        .host
        .iter()
        .filter(|e| matches!(e, ServerSessionEvent::PeerInterrupted { peer: p } if *p == peer))
        .count();
    let restored = pair
        .log
        .host
        .iter()
        .filter(|e| matches!(e, ServerSessionEvent::PeerRestored { peer: p } if *p == peer))
        .count();
    eprintln!("flappy 2 min: {sent} messages, {interrupted} interruptions, {stats:?}");
    assert!(
        interrupted >= 5,
        "{interrupted} interruptions in six {MOCK_FLAP_OUTAGE_MS} ms outages"
    );
    assert_eq!(interrupted, restored, "every interruption ended");
}

fn lifecycle(app: &mut App, event: LifecycleEvent) {
    app.world_mut().resource_mut::<AppLifecycle>().last_event = Some(event);
    app.run_lifecycle();
    app.world_mut().resource_mut::<AppLifecycle>().last_event = None;
}

#[test]
fn a_suspended_steam_client_on_bad_wifi_resumes_over_steam_with_the_same_peer_id() {
    let mut pair = Pair::new(MockLink::profile("bad-wifi").expect("bad-wifi"), 4);
    let peer = pair.launch();
    pair.play(peer, 10, 0);
    // The platform suspends the guest. On bad-wifi its `suspended` goodbye may be lost; the host
    // then sees silence and holds the slot either way.
    lifecycle(&mut pair.guest, LifecycleEvent::Suspended);
    // The phone sleeps 5 s.
    for _ in 0..(5_000 / FRAME_MS) {
        pair.frame(false);
    }
    assert_eq!(pair.host_peers(), vec![peer], "the host held the guest's slot");
    lifecycle(&mut pair.guest, LifecycleEvent::Resumed);
    for _ in 0..1_250 {
        pair.frame(true);
        if pair.guest_state() == ClientSessionState::InGame
            && pair.host_peer_state(peer) == Some(PeerState::InGame)
        {
            break;
        }
    }
    let session = pair.guest.world().resource::<NetSession>();
    assert_eq!(
        session.state(),
        ClientSessionState::InGame,
        "resumed over Steam on bad-wifi"
    );
    // Only a Steam socket reaches this synthetic address: a UDP socket would time out.
    assert_eq!(session.server_addr(), Some(steam_peer_addr(SteamId(HOST), 0)));
    assert_eq!(pair.guest_peer(), Some(peer), "the same PeerId");
    assert_eq!(pair.host_peers(), vec![peer], "no second peer on the host");
    let resumed = pair
        .log
        .host
        .iter()
        .filter(|e| matches!(e, ServerSessionEvent::PlayerResumed { peer: p } if *p == peer))
        .count();
    assert_eq!(resumed, 1, "the host resumed the slot with the reconnect token");
    eprintln!("bad-wifi resume: {:?}", pair.net.link_stats());
    // And the resumed session keeps playing on bad-wifi.
    pair.play(peer, 30, 10_000);
}

/// Sends a `size`-byte message from 21 (on `link`) to 22 (clean) every 10 ms for `ms`
/// milliseconds and returns the send times that arrived.
fn stream_times(net: &MockSteamNet, link: MockLink, ms: u64, size: usize) -> Vec<u64> {
    let a = user(net, 21, "a");
    let b = user(net, 22, "b");
    net.set_link(SteamId(21), link);
    let mut sa = SteamSocket::open(a.p2p().expect("p2p"), 0).expect("socket");
    let mut sb = SteamSocket::open(b.p2p().expect("p2p"), 0).expect("socket");
    let mut buf = vec![0u8; size.max(8)];
    let mut arrived = Vec::new();
    for t in 0..ms {
        if t % 10 == 0 {
            let mut msg = vec![0u8; size.max(8)];
            msg[..8].copy_from_slice(&t.to_le_bytes());
            sa.send_to(&msg, steam_peer_addr(SteamId(22), 0)).expect("send");
        }
        net.advance_ms(1);
        while let Some((n, _)) = sb.recv_from(&mut buf).expect("io") {
            assert!(n >= 8);
            let mut head = [0u8; 8];
            head.copy_from_slice(&buf[..8]);
            arrived.push(u64::from_le_bytes(head));
        }
    }
    arrived
}

/// KI-NT-051 (ADR-10069): `flappy` on every fourth client carries the harness's long outage
/// (the engine's removal timeout plus 2 s), shifted per client like the flaps.
#[test]
fn flappy_on_every_fourth_client_has_the_long_outage() {
    let duration = 60_000;
    let long = regia_net::TimeoutPolicy::default().timeout_ms + regia_steam::MOCK_FLAP_LONG_PAST_TIMEOUT_MS;
    assert!(
        long > regia_net::TimeoutPolicy::default().timeout_ms,
        "it outlasts the removal timeout"
    );
    let index = 3;
    let fourth = MockLink::profile_for_client("flappy", index, duration).expect("flappy");
    let shift = (index as u64 * 137) % 1_000;
    assert_eq!(fourth.outages, vec![(duration / 2 + shift, long)]);
    assert_eq!(
        fourth.flaps,
        Some((
            regia_steam::MOCK_FLAP_FIRST_MS + shift,
            regia_steam::MOCK_FLAP_PERIOD_MS,
            MOCK_FLAP_OUTAGE_MS
        ))
    );
    assert!(
        MockLink::profile_for_client("flappy", 2, duration)
            .expect("flappy")
            .outages
            .is_empty()
    );
    // Only the sender has the link: nothing sent inside the 12 s window arrives.
    let net = MockSteamNet::new();
    let only_outages = MockLink {
        up: MockDirection::default(),
        down: MockDirection::default(),
        ..fourth
    };
    let arrived = stream_times(&net, only_outages, duration, 8);
    let start = duration / 2 + shift;
    assert!(
        arrived.iter().all(|t| !(start..start + long).contains(t)),
        "a message crossed the long outage"
    );
    assert!(
        arrived.iter().any(|t| *t >= start + long),
        "the link came back after the long outage"
    );
    assert!(net.link_stats().outage_drops >= long / 10);
}

/// KI-NT-051 (ADR-10069): `mobile-4g` carries the path MTU (a larger message travels as fragments
/// and is lost when one fragment is) and, per client, one NAT rebinding whose reroute gap drops
/// what is sent in it.
#[test]
fn mobile_4g_fragments_over_the_mtu_and_drops_in_the_rebinding_gap() {
    let link = MockLink::profile("mobile-4g").expect("mobile-4g");
    assert_eq!(link.up.mtu_bytes, regia_steam::MOCK_MOBILE_MTU_BYTES);
    assert_eq!(link.down.mtu_bytes, regia_steam::MOCK_MOBILE_MTU_BYTES);
    // Fragments: a 2,000-byte message is two fragments (Steam sockets take at most 2,048 bytes), so
    // it is lost more often than a small one
    // under the same independent loss.
    let lossy = |mtu: usize| MockLink {
        up: MockDirection {
            loss: 0.2,
            mtu_bytes: mtu,
            ..MockDirection::default()
        },
        ..MockLink::clean()
    };
    let delivered = |mtu: usize, size: usize| {
        let net = MockSteamNet::new();
        net.set_seed(5);
        let n = stream_times(&net, lossy(mtu), 20_000, size).len();
        (n, net.link_stats())
    };
    let (whole, whole_stats) = delivered(0, 2_000);
    let (split, split_stats) = delivered(regia_steam::MOCK_MOBILE_MTU_BYTES, 2_000);
    assert_eq!(whole_stats.fragmented, 0);
    assert_eq!(
        split_stats.fragmented, 2_000,
        "every 2,000-byte message is fragmented"
    );
    assert!(
        split * 10 < whole * 9,
        "two fragments at 20 % loss each deliver about 64 %, one message about 80 %: {split} vs {whole}"
    );
    let (small, small_stats) = delivered(regia_steam::MOCK_MOBILE_MTU_BYTES, 64);
    assert_eq!(
        small_stats.fragmented, 0,
        "a message under the MTU is not fragmented"
    );
    assert!(small * 10 > 2_000 * 7, "{small}");
    // The rebinding of client 1 at 40 % of a 20 s run plus 97 ms.
    let client = MockLink::profile_for_client("mobile-4g", 1, 20_000).expect("mobile-4g");
    assert_eq!(client.rebinds_ms, vec![8_000 + 97]);
    assert_eq!(client.rebind_gap_ms, regia_steam::MOCK_REBIND_GAP_MS);
    let net = MockSteamNet::new();
    let rebinding = MockLink {
        rebinds_ms: client.rebinds_ms.clone(),
        rebind_gap_ms: client.rebind_gap_ms,
        ..MockLink::clean()
    };
    let arrived = stream_times(&net, rebinding, 20_000, 8);
    let gap = 8_097..8_097 + regia_steam::MOCK_REBIND_GAP_MS;
    assert!(
        arrived.iter().all(|t| !gap.contains(t)),
        "a message crossed the reroute gap"
    );
    assert_eq!(arrived.len() as u64 + net.link_stats().rebind_drops, 2_000);
    assert_eq!(
        net.link_stats().rebind_drops,
        regia_steam::MOCK_REBIND_GAP_MS / 10
    );
}
