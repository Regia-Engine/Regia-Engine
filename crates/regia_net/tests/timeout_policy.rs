// SPDX-License-Identifier: MPL-2.0
//! Deterministic outage regressions (spec 120 §§ 10.1–10.2, ADR-7950).
use regia_net::connection::{Connection, LinkConfig};
use regia_net::prelude::*;
use regia_net::session::{ClientSessionState, ConnectOptions, HostOptions, NetSession, SessionSettings};
use regia_net::socket::LoopbackSocket;
use std::io;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// The default link with the first flight named off (ADR-10156; off by default in v0.7,
/// ADR-10231, and a later release may turn it on): these
/// tests measure the transport's blind start (the 64-packet initial window and what it makes of
/// acknowledgments, bursts and rates), which the first flight replaces until a path answers.
fn blind_link() -> LinkConfig {
    let mut config = LinkConfig::default();
    config.rate.first_flight = false;
    config
}

struct GateSocket {
    socket: LoopbackSocket,
    closed: Arc<AtomicBool>,
}
impl DatagramSocket for GateSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            Ok(())
        } else {
            self.socket.send_to(bytes, dest)
        }
    }
    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.socket.recv_from(buf)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}
fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// The reliable fragment ids a wire-5 packet body carries (spec 120 § 4.1 layout: flags, packet
/// number, optional acknowledgment frame, records).
fn reliable_ids(p: &[u8]) -> Vec<u16> {
    use regia_net::wire::WireReader;
    let mut ids = Vec::new();
    let mut r = WireReader::new(p);
    let Ok(flags) = r.u8() else {
        return ids;
    };
    if r.take([1, 2, 4, 4][usize::from(flags & 0x03)]).is_err() {
        return ids;
    }
    if flags & 0x04 != 0 && regia_net::ack::AckFrame::decode(&mut r, flags & 0x08 != 0).is_err() {
        return ids;
    }
    while let Ok(tag) = r.u8() {
        match tag {
            0x01 => {
                let Ok(len) = r.u16() else { break };
                if r.take(usize::from(len)).is_err() {
                    break;
                }
            }
            0x02 | 0x82 => {
                let (Ok(id), Ok(len)) = (r.u16(), r.u16()) else {
                    break;
                };
                if r.take(usize::from(len)).is_err() {
                    break;
                }
                ids.push(id);
            }
            _ => {}
        }
    }
    ids
}

/// Keepalives far apart, so only the probe timeout sends (wire 5 keepalives are pings that would
/// postpone it, ADR-8184).
fn probe_only() -> LinkConfig {
    LinkConfig {
        keepalive_ms: 1_000_000,
        ..LinkConfig::default()
    }
}

#[test]
fn defaults_allow_twenty_seconds_of_silence() {
    assert_eq!(ClientConfig::default().timeout_ms, 20_000);
    assert_eq!(regia_net::session::HealthPolicy::default().timeout_ms, 20_000);
    assert_eq!(SessionSettings::default().timeout_s, 20.0);
}

#[test]
fn fifty_fragments_back_off_during_eight_second_outage() {
    let mut a = Connection::new(blind_link());
    for id in 0..50u8 {
        a.send(Channel::GAME_RPC, vec![id; 1100], 0).expect("queue");
    }
    let initial = a.poll(0, &[]);
    assert_eq!(initial.len(), 50);
    // Count actual fragment transmissions, not an average that could hide one storming fragment.
    let mut resends = [0u32; 50];
    for now in (10..=8000).step_by(10) {
        for packet in a.poll(now, &[]) {
            let mut receiver = Connection::new(blind_link());
            // Every reliable record of the packet (wire 5 layout), not only the first.
            for id in reliable_ids(&packet) {
                let id = usize::from(id);
                assert!(id < 50);
                resends[id] += 1;
            }
            receiver.receive(&packet, now).expect("valid packet");
        }
    }
    println!(
        "eight_second_outage: max={} total={} per_fragment={resends:?}",
        resends.iter().max().expect("50"),
        resends.iter().sum::<u32>()
    );
    assert!(
        resends.iter().all(|n| *n <= 5),
        "resends per fragment: {resends:?}"
    );
}

#[test]
fn outage_recovery_limits_the_first_poll_to_four_packets() {
    let mut a = Connection::new(blind_link());
    for id in 0..50u8 {
        a.send(Channel::GAME_RPC, vec![id; 1100], 0).expect("queue");
    }
    a.poll(0, &[]);
    let packets = a.poll(2500, &[]);
    assert!(
        packets.len() <= 4,
        "recovery first poll sent {} packets",
        packets.len()
    );
}

#[test]
fn net_session_thirty_second_policy_survives_twenty_five_second_outage() {
    let net = LoopbackNet::new();
    let closed = Arc::new(AtomicBool::new(false));
    let socket = |port| GateSocket {
        socket: net.bind(addr(port)).expect("bind"),
        closed: closed.clone(),
    };
    let settings = SessionSettings {
        timeout_s: 30.0,
        ..SessionSettings::default()
    };
    let mut server = NetSession::default();
    let mut client = NetSession::default();
    server.set_settings(settings.clone());
    client.set_settings(settings);
    server.set_manual_clock(0);
    client.set_manual_clock(0);
    server
        .host_on(socket(7777), HostOptions::default(), true)
        .expect("host");
    client
        .connect_on(socket(0), addr(7777), ConnectOptions::default())
        .expect("join");
    let step = |server: &mut NetSession, client: &mut NetSession| {
        server.advance_manual_clock(10);
        client.advance_manual_clock(10);
        client.update();
        server.update();
    };
    for _ in 0..200 {
        step(&mut server, &mut client);
    }
    assert_eq!(client.state(), ClientSessionState::InGame);
    server.take_server_events();
    client.take_client_events();
    closed.store(true, Ordering::Relaxed);
    for _ in 0..2500 {
        step(&mut server, &mut client);
    }
    assert_eq!(
        client.state(),
        ClientSessionState::InGame,
        "seat must survive the outage"
    );
    closed.store(false, Ordering::Relaxed);
    for _ in 0..1000 {
        step(&mut server, &mut client);
    }
    assert_eq!(client.state(), ClientSessionState::InGame);
    let server_events = server.take_server_events();
    let client_events = client.take_client_events();
    use regia_net::session::{ClientSessionEvent, ServerSessionEvent};
    assert_eq!(
        server_events
            .iter()
            .filter(|e| matches!(e, ServerSessionEvent::PeerInterrupted { .. }))
            .count(),
        1
    );
    assert_eq!(
        server_events
            .iter()
            .filter(|e| matches!(e, ServerSessionEvent::PeerRestored { .. }))
            .count(),
        1
    );
    assert_eq!(
        client_events
            .iter()
            .filter(|e| matches!(e, ClientSessionEvent::PeerInterrupted))
            .count(),
        1
    );
    assert_eq!(
        client_events
            .iter()
            .filter(|e| matches!(e, ClientSessionEvent::PeerRestored))
            .count(),
        1
    );
}

#[derive(Debug)]
struct Game;
impl NetGame for Game {
    type Message = u32;
    type Input = u32;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.timeouts";
    const PROTOCOL_VERSION: u32 = 1;
}
type Server = NetServer<Game, GateSocket>;
type Client = NetClient<Game, GateSocket>;

struct Pair {
    server: Server,
    client: Client,
    gate: Arc<AtomicBool>,
    now: u64,
    peer: PeerId,
    server_events: Vec<String>,
    client_events: Vec<String>,
    to_server: Vec<u32>,
    to_client: Vec<u32>,
}
impl Pair {
    fn new(timeout_ms: u64, start_game: bool) -> Self {
        let net = LoopbackNet::new();
        let gate = Arc::new(AtomicBool::new(false));
        let socket = |port| GateSocket {
            socket: net.bind(addr(port)).expect("bind"),
            closed: gate.clone(),
        };
        let server = NetServer::with_secret(
            socket(7777),
            ServerConfig {
                health: regia_net::session::HealthPolicy {
                    timeout_ms,
                    ..Default::default()
                },
                crypto: regia_net::crypto::CryptoMode::Required,
                ..Default::default()
            },
            [5; 32],
        );
        let client = NetClient::connect(
            socket(0),
            addr(7777),
            ClientConfig {
                timeout_ms,
                crypto: regia_net::crypto::CryptoMode::Required,
                ..Default::default()
            },
            &(),
        )
        .expect("connect");
        let mut p = Self {
            server,
            client,
            gate,
            now: 0,
            peer: PeerId(0),
            server_events: vec![],
            client_events: vec![],
            to_server: vec![],
            to_client: vec![],
        };
        for _ in 0..100 {
            p.step();
        }
        p.peer = p.client.peer_id().expect("connected");
        if start_game {
            p.client.send(Channel::GAME_RPC, &u32::MAX).expect("send");
            p.server.send(p.peer, Channel::GAME_RPC, &u32::MAX).expect("send");
            for _ in 0..100 {
                p.step();
            }
        }
        p.server_events.clear();
        p.client_events.clear();
        p.to_server.clear();
        p.to_client.clear();
        p
    }
    fn step(&mut self) {
        self.now += 10;
        self.client.receive(self.now);
        self.server.receive(self.now);
        self.client.flush(self.now);
        self.server.flush(self.now);
        while let Some(e) = self.client.poll_event() {
            if let ClientEvent::Message { message, .. } = e {
                self.to_client.push(message);
            } else {
                self.client_events.push(format!("{e:?}"));
            }
        }
        while let Some(e) = self.server.poll_event() {
            if let ServerEvent::Message { message, .. } = e {
                self.to_server.push(message);
            } else {
                self.server_events.push(format!("{e:?}"));
            }
        }
    }
    fn assert_alive(&self) {
        assert!(self.client.is_connected(), "client {:?}", self.client_events);
        assert!(
            self.server.session().peer(self.peer).is_some(),
            "server {:?}",
            self.server_events
        );
    }
}
fn event_count(events: &[String], name: &str) -> usize {
    events.iter().filter(|e| e.starts_with(name)).count()
}

#[test]
fn interrupted_and_restored_once_per_outage_on_both_ends() {
    let mut p = Pair::new(30_000, true);
    p.gate.store(true, Ordering::Relaxed);
    for _ in 0..140 {
        p.step();
    }
    assert_eq!(event_count(&p.client_events, "PeerInterrupted"), 0);
    assert_eq!(event_count(&p.server_events, "PeerInterrupted"), 0);
    for _ in 0..400 {
        p.step();
    }
    p.assert_alive();
    assert_eq!(event_count(&p.client_events, "PeerInterrupted"), 1);
    assert_eq!(event_count(&p.server_events, "PeerInterrupted"), 1);
    assert!(
        ron::to_string(&p.client.status())
            .expect("status")
            .contains("interrupted:true")
    );
    assert!(
        ron::to_string(&p.server.peer_rows())
            .expect("peers")
            .contains("interrupted:true")
    );
    assert!(
        p.server
            .overlay()
            .lines()
            .iter()
            .any(|line| line.contains("INTERRUPTED"))
    );
    p.gate.store(false, Ordering::Relaxed);
    for _ in 0..200 {
        p.step();
    }
    assert_eq!(event_count(&p.client_events, "PeerRestored"), 1);
    assert_eq!(event_count(&p.server_events, "PeerRestored"), 1);
}

#[test]
fn start_grace_keeps_a_peer_that_has_sent_no_game_data() {
    let mut p = Pair::new(10_000, false);
    p.gate.store(true, Ordering::Relaxed);
    for _ in 0..2500 {
        p.step();
    }
    p.assert_alive();
    for _ in 0..600 {
        p.step();
    }
    assert_eq!(event_count(&p.server_events, "PeerLeft"), 1);
}

// Datagram-level good-wifi numbers from the net-soak-harness profile table. At this test's
// small offered load the 20/50 Mbit/s bottleneck is not exercised; no harness dependency is added.
fn good_wifi_profile() -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        delay_ms: 4,
        jitter_ms: 4,
        loss: 0.002,
        reorder: 0.001,
        reorder_max_ms: 10,
        duplicate: 0.0005,
        ..Default::default()
    }
}

#[test]
fn flappy_five_minutes_delivers_every_message_once_in_order() {
    // Preserve the initial brief-phase case and also exercise the harness's t=15 s offset.
    for (first_outage_ms, outages) in [(20_000u64, 14usize), (15_000, 15)] {
        let mut p = Pair::new(20_000, true);
        p.server.set_conditioner(good_wifi_profile());
        p.client.set_conditioner(ConditionerConfig {
            seed: 2,
            ..good_wifi_profile()
        });
        let start = p.now;
        let mut expected = Vec::new();
        for tick in 0..30_000u32 {
            let elapsed = p.now - start;
            p.gate.store(
                elapsed >= first_outage_ms && (elapsed - first_outage_ms) % 20_000 < 3000,
                Ordering::Relaxed,
            );
            if tick.is_multiple_of(10) {
                expected.push(tick);
                p.client.send(Channel::GAME_RPC, &tick).expect("send client");
                p.server
                    .send(p.peer, Channel::GAME_RPC, &tick)
                    .expect("send server");
            }
            p.step();
            p.assert_alive();
        }
        p.gate.store(false, Ordering::Relaxed);
        for _ in 0..1000 {
            p.step();
        }
        assert_eq!(p.to_client, expected);
        assert_eq!(p.to_server, expected);
        for events in [&p.server_events, &p.client_events] {
            assert_eq!(event_count(events, "PeerInterrupted"), outages);
            assert_eq!(event_count(events, "PeerRestored"), outages);
            assert_eq!(event_count(events, "PeerLeft") + event_count(events, "Ended"), 0);
        }
    }
}

#[test]
fn good_wifi_ten_minutes_never_interrupts() {
    let wifi = ConditionerConfig {
        enabled: true,
        delay_ms: 20,
        jitter_ms: 10,
        loss: 0.005,
        ..Default::default()
    };
    for wifi in [wifi, good_wifi_profile()] {
        let mut p = Pair::new(20_000, true);
        p.server.set_conditioner(wifi.clone());
        p.client.set_conditioner(ConditionerConfig { seed: 2, ..wifi });
        for _ in 0..60_000 {
            p.step();
            p.assert_alive();
        }
        assert_eq!(event_count(&p.client_events, "PeerInterrupted"), 0);
        assert_eq!(event_count(&p.server_events, "PeerInterrupted"), 0);
    }
}

#[test]
fn three_second_rtt_twenty_percent_loss_is_never_a_kick_reason() {
    let mut p = Pair::new(20_000, true);
    let extreme = ConditionerConfig {
        enabled: true,
        delay_ms: 1500,
        loss: 0.2,
        ..Default::default()
    };
    p.server.set_conditioner(extreme.clone());
    p.client.set_conditioner(ConditionerConfig { seed: 2, ..extreme });
    for _ in 0..60_000 {
        p.step();
        p.assert_alive();
    }
}

#[derive(Debug)]
struct LinkGame;
impl NetGame for LinkGame {
    type Message = regia_net::bridge::NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.link_timeout";
    const PROTOCOL_VERSION: u32 = 1;
}
#[test]
fn net_link_thirty_second_policy_survives_long_outage_burst() {
    use regia_app::App;
    use regia_net::link::{LinkClock, LinkEvent, LinkOptions, NetLink, NetLinkPlugin};
    use regia_net::{NetMode, NetPlugin};
    let net = LoopbackNet::new();
    let app = |mode| {
        let mut app = App::new();
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
        app
    };
    let mut server = app(NetMode::DedicatedServer);
    let mut client = app(NetMode::Client);
    let options = LinkOptions {
        timeout: regia_net::session::TimeoutPolicy {
            timeout_ms: 30_000,
            ..Default::default()
        },
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: 10_000,
        },
        conditioner: ConditionerConfig {
            enabled: true,
            burst_start_ms: 2_000,
            burst_len_ms: 25_000,
            burst_loss: 1.0,
            ..Default::default()
        },
        ..Default::default()
    };
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<LinkGame>(Box::new(net.bind(addr(7778)).expect("bind")), &options)
        .expect("host");
    client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect::<LinkGame>(Box::new(net.bind(addr(0)).expect("bind")), addr(7778), &options)
        .expect("join");
    for _ in 0..200 {
        server.update();
        client.update();
    }
    assert!(client.world().resource::<NetLink>().is_connected());
    let peer = server.world().resource::<regia_net::runtime::NetNode>().peers()[0];
    let pawn = server.world_mut().spawn(regia_net::NetOwner(peer)).id();
    let other = server.world_mut().spawn(regia_net::NetOwner(PeerId::SERVER)).id();
    for _ in 0..2500 {
        server.update();
        client.update();
        assert!(client.world().resource::<NetLink>().is_connected());
        assert_eq!(
            server
                .world()
                .resource::<regia_net::runtime::NetNode>()
                .peers()
                .len(),
            1
        );
    }
    assert!(server.world().get::<regia_net::NetInterrupted>(pawn).is_some());
    assert!(server.world().get::<regia_net::NetInterrupted>(other).is_none());
    for _ in 0..1000 {
        server.update();
        client.update();
    }
    assert!(server.world().get::<regia_net::NetInterrupted>(pawn).is_none());
    for app in [&mut server, &mut client] {
        let events = app.world_mut().resource_mut::<NetLink>().take_events();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, LinkEvent::PeerInterrupted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, LinkEvent::PeerRestored { .. }))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LinkEvent::Ended { .. } | LinkEvent::Left { .. }))
        );
        // ADR-8182: the link's grade goes to Interrupted once and back once.
        let to_interrupted = events
            .iter()
            .filter(|e| {
                matches!(e, LinkEvent::QualityChanged { to, .. } if *to == regia_net::LinkGrade::Interrupted)
            })
            .count();
        let from_interrupted = events
            .iter()
            .filter(|e| {
                matches!(e, LinkEvent::QualityChanged { from, .. } if *from == regia_net::LinkGrade::Interrupted)
            })
            .count();
        assert_eq!((to_interrupted, from_interrupted), (1, 1), "{events:?}");
    }
}

#[test]
fn three_and_eight_second_stalls_keep_the_seat_twelve_removes_at_ten_seconds() {
    for stall_ms in [3_000, 8_000, 12_000] {
        let mut p = Pair::new(10_000, true);
        p.gate.store(true, Ordering::Relaxed);
        // Deliver the datagram already queued before closing the path, then measure the actual
        // final authenticated receive. No tolerance is added to the removal deadline.
        p.step();
        let last = p.server.session().peer(p.peer).expect("peer").last_seen_ms;
        let start = p.now;
        let mut removed_at = None;
        while p.now - start < stall_ms {
            p.step();
            if p.server.session().peer(p.peer).is_none() && removed_at.is_none() {
                removed_at = Some(p.now);
            }
        }
        if stall_ms < 10_000 {
            p.assert_alive();
            p.gate.store(false, Ordering::Relaxed);
            for _ in 0..200 {
                p.step();
            }
            p.assert_alive();
            assert_eq!(event_count(&p.server_events, "PeerRestored"), 1);
        } else {
            assert_eq!(removed_at, Some(last + 10_000));
            assert_eq!(event_count(&p.server_events, "PeerLeft"), 1);
        }
    }
}

#[test]
fn silence_removes_exactly_the_silent_peer_at_the_boundary() {
    use regia_net::session::{HealthPolicy, Session};
    let mut session = Session::new(
        8,
        HealthPolicy {
            timeout_ms: 10_000,
            start_grace_ms: 0,
            ..Default::default()
        },
    );
    let quiet = session
        .add_peer(addr(1), "quiet".into(), PeerRole::Player, 0)
        .expect("peer");
    let live = session
        .add_peer(addr(2), "live".into(), PeerRole::Player, 0)
        .expect("peer");
    for now in (0..=12_000).step_by(10) {
        session.touch(live, now);
        let dead = session.timed_out(now);
        if now == 10_000 {
            assert_eq!(dead, [quiet]);
            session.remove_peer(quiet);
        } else {
            assert!(dead.is_empty());
        }
        assert!(session.peer(live).is_some());
    }
}

#[test]
fn first_second_after_recovery_is_bounded_independently_of_poll_frequency() {
    use regia_net::connection::RECOVERY_PACKETS_PER_SECOND;
    let mut a = Connection::new(blind_link());
    let mut b = Connection::new(blind_link());
    for id in 0..50u8 {
        a.send(Channel::GAME_RPC, vec![id; 1100], 0).expect("queue");
    }
    a.poll(0, &[]); // all lost
    // The path returns at t=2500. Polling repeatedly at one timestamp cannot drain the backlog.
    let mut reliable = 0;
    let mut total = 0;
    for now in 2500..3500 {
        for packet in a.poll(now, &[]) {
            total += 1;
            reliable += usize::from(!reliable_ids(&packet).is_empty());
            b.receive(&packet, now).expect("data");
        }
        for packet in b.poll(now, &[]) {
            a.receive(&packet, now).expect("ack");
        }
    }
    println!(
        "first_recovery_second: reliable={reliable}, total_datagrams={total}, bound={RECOVERY_PACKETS_PER_SECOND}"
    );
    assert!(reliable <= RECOVERY_PACKETS_PER_SECOND);
    assert!(
        total <= RECOVERY_PACKETS_PER_SECOND + 10,
        "all recovery packets: {total}"
    );
    for now in 3500..10_000 {
        for packet in a.poll(now, &[]) {
            b.receive(&packet, now).expect("data");
        }
        for packet in b.poll(now, &[]) {
            a.receive(&packet, now).expect("ack");
        }
    }
    let delivered: Vec<_> = std::iter::from_fn(|| b.next_message())
        .map(|(_, bytes)| bytes[0])
        .collect();
    assert_eq!(delivered, (0..50).collect::<Vec<u8>>());
    assert_eq!(a.stats().reliable_in_flight, 0);
}

#[test]
fn fresh_ack_resets_backoff_and_duplicate_ack_does_not() {
    // Wire 5 (ADR-8184): one probe timeout replaces the per-fragment timers. Only it sends here.
    let mut a = Connection::new(probe_only());
    let mut b = Connection::new(probe_only());
    a.send(Channel::GAME_RPC, vec![7; 1100], 0).expect("queue");
    for now in (0..=8000).step_by(10) {
        a.poll(now, &[]);
    }
    // Probes at 100, 300, 700, 1500, 3100 and 6300 ms (the wait doubles from 100 ms).
    let before = a.stats().reliable_resends;
    assert_eq!(before, 6);
    assert_eq!(a.stats().probe_timeouts, 6);
    a.send(Channel::Unreliable, vec![9], 0).expect("probe");
    for packet in a.poll(8100, &[]) {
        b.receive(&packet, 8100).expect("probe");
    }
    let ack = b.poll(8110, &[]).remove(0);
    a.receive(&ack, 8120).expect("ack");
    a.poll(8120, &[]);
    assert_eq!(
        a.stats().reliable_resends,
        before + 1,
        "a fresh ACK ends the 6.4 s wait: the fragment is repaired at once"
    );
    // The probe timeout starts again from its base (100 ms): probes at 8220 and 8420. A
    // duplicate of the old ACK at 8300 must not reset it (that would probe at 8400).
    let mut probes = Vec::new();
    for now in (8130..=8600).step_by(10) {
        if now == 8300 {
            a.receive(&ack, now).expect("duplicate ack");
        }
        if a.poll(now, &[]).iter().any(|p| !reliable_ids(p).is_empty()) {
            probes.push(now);
        }
    }
    assert_eq!(probes, [8220, 8420], "duplicate ACK cannot reset the wait");
    assert_eq!(a.stats().reliable_resends, before + 3);
}

#[test]
fn repeated_timeout_waits_stop_growing_at_eight_seconds() {
    let mut a = Connection::new(probe_only());
    a.send(Channel::GAME_RPC, vec![7; 1100], 0).expect("queue");
    let mut times = Vec::new();
    for now in (0..=40_000).step_by(10) {
        for packet in a.poll(now, &[]) {
            if !reliable_ids(&packet).is_empty() {
                times.push(now);
            }
        }
    }
    // The probe timeout doubles from 100 ms and stops at `resend_backoff_max_ms` (8 s).
    assert_eq!(
        &times[..8],
        &[0, 100, 300, 700, 1500, 3100, 6300, 12_700],
        "{times:?}"
    );
    assert!(times[7..].windows(2).all(|t| t[1] - t[0] == 8000), "{times:?}");
    // With the default keepalives, the pings probe the path instead and the data is not
    // repeated more than once in 40 s of silence.
    let mut a = Connection::new(LinkConfig::default());
    a.send(Channel::GAME_RPC, vec![7; 1100], 0).expect("queue");
    let mut data = 0;
    for now in (0..=40_000).step_by(10) {
        data += a
            .poll(now, &[])
            .iter()
            .filter(|p| !reliable_ids(p).is_empty())
            .count();
    }
    assert!(data <= 2, "{data} data packets in 40 s of silence");
}

struct RoutedSocket {
    socket: LoopbackSocket,
    impaired: SocketAddr,
    conditioner: regia_net::NetConditioner,
    now: Arc<std::sync::atomic::AtomicU64>,
}
impl DatagramSocket for RoutedSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if dest == self.impaired {
            for (bytes, dest) in
                self.conditioner
                    .process(bytes.to_vec(), dest, self.now.load(Ordering::Relaxed))
            {
                self.socket.send_to(&bytes, dest)?;
            }
            Ok(())
        } else {
            self.socket.send_to(bytes, dest)
        }
    }
    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        for (bytes, dest) in self.conditioner.release_due(self.now.load(Ordering::Relaxed)) {
            self.socket.send_to(&bytes, dest)?;
        }
        self.socket.recv_from(buf)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

#[test]
fn rtt_extreme_peer_of_eight_survives_ten_minutes() {
    use std::sync::atomic::AtomicU64;
    let net = LoopbackNet::new();
    let now = Arc::new(AtomicU64::new(0));
    let extreme = ConditionerConfig {
        enabled: true,
        delay_ms: 1500,
        loss: 0.2,
        seed: 120,
        ..Default::default()
    };
    let socket = RoutedSocket {
        socket: net.bind(addr(7777)).expect("bind"),
        impaired: addr(8000),
        conditioner: regia_net::NetConditioner::new(extreme.clone()),
        now: now.clone(),
    };
    let mut server: NetServer<Game, _> = NetServer::with_secret(socket, ServerConfig::default(), [5; 32]);
    let mut clients: Vec<NetClient<Game, _>> = (0..8)
        .map(|i| {
            NetClient::connect(
                net.bind(addr(8000 + i)).expect("bind"),
                addr(7777),
                ClientConfig {
                    connect_timeout_ms: 30_000,
                    conditioner: if i == 0 {
                        ConditionerConfig {
                            seed: 121,
                            ..extreme.clone()
                        }
                    } else {
                        ConditionerConfig::default()
                    },
                    ..Default::default()
                },
                &(),
            )
            .expect("connect")
        })
        .collect();
    let mut tick = 0u64;
    let step = |tick: &mut u64,
                server: &mut NetServer<Game, RoutedSocket>,
                clients: &mut Vec<NetClient<Game, LoopbackSocket>>| {
        *tick += 10;
        now.store(*tick, Ordering::Relaxed);
        for client in clients.iter_mut() {
            client.receive(*tick);
            client.flush(*tick);
        }
        server.receive(*tick);
        server.flush(*tick);
    };
    for _ in 0..6000 {
        step(&mut tick, &mut server, &mut clients);
        if clients.iter().all(NetClient::is_connected) {
            break;
        }
    }
    assert!(clients.iter().all(NetClient::is_connected));
    assert_eq!(server.session().len(), 8);
    while server.poll_event().is_some() {}
    for _ in 0..60_000 {
        step(&mut tick, &mut server, &mut clients);
        assert_eq!(server.session().len(), 8);
        assert!(clients.iter().all(NetClient::is_connected));
        while let Some(event) = server.poll_event() {
            assert!(!matches!(event, ServerEvent::PeerLeft { .. }), "{event:?}");
        }
        for client in &mut clients {
            while let Some(event) = client.poll_event() {
                assert!(!matches!(event, ClientEvent::Ended(_)), "{event:?}");
            }
        }
    }
}

#[test]
fn interrupted_marker_replicates_and_restores_without_despawning() {
    use regia_ecs::prelude::*;
    use regia_net::runtime::NetEntities;
    use regia_net::runtime::server_set_peer_interrupted;
    use regia_net::testing::LoopbackHub;
    use regia_net::{
        NetId, NetInterrupted, NetMode, NetOwner, ReplicateOptions, Replicated, TransformReplication,
    };
    // The marker replicates only for a game that opts in on every process (protocol hash).
    fn setup(world: &mut World) {
        regia_net::replication::register_replicated::<NetInterrupted>(world, ReplicateOptions::default());
    }
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 120, setup);
    let owner = hub.add_client();
    let c = hub.add_client();
    let peer = hub.clients[owner].peer;
    // The marker follows the owner's interruption (a marker on an entity no interrupted peer owns
    // is reconciled away before the gather).
    let entity = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::None,
                ..Default::default()
            },
            NetOwner(peer),
        ))
        .id();
    hub.run(10);
    let id = *hub.server.world.get::<NetId>(entity).expect("assigned");
    let copy = hub.clients[c]
        .world
        .resource::<NetEntities>()
        .entity(id)
        .expect("replica");
    server_set_peer_interrupted(&mut hub.server.world, peer, true);
    hub.run(10);
    assert!(hub.server.world.get::<NetInterrupted>(entity).is_some());
    assert!(hub.clients[c].world.get::<NetInterrupted>(copy).is_some());
    server_set_peer_interrupted(&mut hub.server.world, peer, false);
    hub.run(10);
    assert!(hub.clients[c].world.get::<NetInterrupted>(copy).is_none());
    assert_eq!(
        hub.clients[c].world.resource::<NetEntities>().entity(id),
        Some(copy)
    );
}

#[test]
fn twelve_second_stall_ejects_only_that_transport_peer_at_exact_timeout() {
    use std::sync::atomic::AtomicU64;
    let net = LoopbackNet::new();
    let now = Arc::new(AtomicU64::new(0));
    let outage = ConditionerConfig {
        enabled: true,
        burst_start_ms: 2_000,
        burst_len_ms: 12_000,
        burst_loss: 1.0,
        ..Default::default()
    };
    let socket = RoutedSocket {
        socket: net.bind(addr(7777)).expect("bind"),
        impaired: addr(8000),
        conditioner: regia_net::NetConditioner::new(outage.clone()),
        now: now.clone(),
    };
    let mut server: NetServer<Game, _> = NetServer::with_secret(
        socket,
        ServerConfig {
            health: regia_net::session::HealthPolicy {
                timeout_ms: 10_000,
                start_grace_ms: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        [5; 32],
    );
    let mut bad: NetClient<Game, _> = NetClient::connect(
        net.bind(addr(8000)).expect("bind"),
        addr(7777),
        ClientConfig {
            conditioner: outage,
            timeout_ms: 10_000,
            ..Default::default()
        },
        &(),
    )
    .expect("join");
    let mut good: NetClient<Game, _> = NetClient::connect(
        net.bind(addr(8001)).expect("bind"),
        addr(7777),
        ClientConfig::default(),
        &(),
    )
    .expect("join");
    let mut bad_peer = None;
    let mut last_heard = 0;
    let mut removed = Vec::new();
    for tick in (10..=16_000).step_by(10) {
        now.store(tick, Ordering::Relaxed);
        bad.receive(tick);
        good.receive(tick);
        bad.flush(tick);
        good.flush(tick);
        server.receive(tick);
        server.flush(tick);
        if tick == 1000 {
            assert!(bad.is_connected() && good.is_connected());
            bad_peer = bad.peer_id();
        }
        if let Some(peer) = bad_peer {
            if let Some(p) = server.session().peer(peer) {
                last_heard = p.last_seen_ms;
            }
            assert!(
                server
                    .session()
                    .peer(good.peer_id().expect("healthy connected"))
                    .is_some()
            );
            assert!(good.is_connected());
        }
        while let Some(event) = server.poll_event() {
            if let ServerEvent::PeerLeft { peer, reason } = event {
                assert_eq!(Some(peer), bad_peer);
                assert_eq!(reason, regia_net::DisconnectReason::Timeout);
                assert_eq!(tick, last_heard + 10_000);
                removed.push(peer);
            }
        }
    }
    assert_eq!(removed, [bad_peer.expect("joined")]);
    assert_eq!(server.session().len(), 1);
}

#[test]
fn net_session_start_grace_keeps_an_admitted_slow_loader() {
    use regia_net::session::PeerState;
    // Review row 10: the grace starts at admission. A link that never sends `Hello` gets none
    // (`transport_unify_session.rs`); an admitted loader (Hello sent, Welcome received, no game
    // data yet) is kept past the 20 s silence timeout until the 30 s grace has passed.
    let net = LoopbackNet::new();
    let closed = Arc::new(AtomicBool::new(false));
    let socket = |port| GateSocket {
        socket: net.bind(addr(port)).expect("bind"),
        closed: closed.clone(),
    };
    let mut server = NetSession::default();
    let mut loader = NetSession::default();
    // The level never finishes loading: the loader stays in `Loading` and sends no game data.
    loader.auto_level_ready = false;
    server.set_manual_clock(0);
    loader.set_manual_clock(0);
    server
        .host_on(socket(7777), HostOptions::default(), true)
        .expect("host");
    loader
        .connect_on(socket(0), addr(7777), ConnectOptions::default())
        .expect("join");
    let step = |server: &mut NetSession, loader: &mut NetSession| {
        server.advance_manual_clock(10);
        loader.advance_manual_clock(10);
        loader.update();
        server.update();
    };
    for _ in 0..200 {
        step(&mut server, &mut loader);
    }
    assert_eq!(loader.state(), ClientSessionState::Loading);
    let core = server.server().expect("server");
    assert_eq!(core.handshaking(), 0, "admitted");
    let peer = core.peer_ids()[0];
    assert_eq!(core.peer_state(peer), Some(PeerState::Loading));
    // 25 s of silence from t = 2 s: past the 20 s timeout, inside the grace from admission.
    closed.store(true, Ordering::Relaxed);
    for _ in 0..2500 {
        step(&mut server, &mut loader);
    }
    assert_eq!(
        server.server().expect("server").peer_state(peer),
        Some(PeerState::Loading),
        "kept by the start grace"
    );
    // Past the grace the silent loader is removed.
    for _ in 0..600 {
        step(&mut server, &mut loader);
    }
    assert_eq!(server.server().expect("server").peer_state(peer), None);
}
