// SPDX-License-Identifier: MPL-2.0
//! `NetSession` reconnects through the transport's one-round-trip resume (wire 6, part 3;
//! ADR-10088, spec 120 § 10.3).
//!
//! Until this part a session host's transport kept no tickets, so every session reconnect was a
//! full transport handshake of three round trips followed by the session's own token resume. Now
//! the host's transport keeps a timed-out player's ticket for the session's reconnect grace and
//! the client presents the ticket and the resumption credential on every `resume: true`
//! connect. On the in-memory network, with a virtual clock (no wall clock, no sleeps):
//!
//! - an outage: the automatic reconnect is a one-round-trip transport resume, the session token
//!   resumes the same `PeerId`, and a second outage presents the credential of the new accept;
//! - an early return (the client reconnects before the host's timeout): the transport takes the
//!   live connection over under the same link, and the session resumes the player as a
//!   takeover does, with no drop in between;
//! - the fallback the spec names: a plain link holds no credential and comes back through the
//!   full handshake with its ticket;
//! - the session core, link by link: a link the transport resumed in place waits for its hello;
//!   anything but the old player's token on it is the loss of the old link;
//! - a suspend: the session's own close of the link holds the transport ticket, so `resume_on`
//!   is one round trip too;
//! - the transport: `NetServer::forget_dormant` ends a hold nothing above has a use for, and
//!   `NetServer::kick_held` removes a connection while holding its peer.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_net::NetGame;
use regia_net::client::{ClientConfig, NetClient};
use regia_net::crypto::CryptoMode;
use regia_net::model::PeerId;
use regia_net::server::{NetServer, ServerConfig, ServerEvent};
use regia_net::session::NetSession;
use regia_net::session::lifecycle::{
    ClientSessionEvent, ClientSessionState, HostOptions, LinkId, PeerState, ServerSessionEvent,
    SessionServer, SessionSettings,
};
use regia_net::session::manifest::{ContentManifest, ManifestAsset};
use regia_net::session::net_session::ConnectOptions;
use regia_net::session::proto::{DisconnectReason, Hello, SessionMsg};
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};
use regia_net::status::EndpointTotals;

/// Frame step of every scenario, milliseconds.
const STEP_MS: u64 = 16;
/// One-way delay of every socket, milliseconds.
const DELAY_MS: u64 = 5;

/// The virtual clock every socket and session of a scenario reads.
#[derive(Debug, Clone, Default)]
struct Clock(Arc<AtomicU64>);

impl Clock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    fn advance(&self, ms: u64) -> u64 {
        self.0.fetch_add(ms, Ordering::Relaxed) + ms
    }
}

/// Switches a test flips on a socket.
#[derive(Debug, Clone, Default)]
struct Switches {
    /// Drop everything this socket sends.
    mute: Arc<AtomicBool>,
    /// Drop everything this socket receives.
    deaf: Arc<AtomicBool>,
    /// Send at once, with no delay: for a client whose frames are about to stop (a suspend), so
    /// nothing waits in this socket for a call that never comes.
    instant: Arc<AtomicBool>,
}

impl Switches {
    fn cut(&self, on: bool) {
        self.mute.store(on, Ordering::Relaxed);
        self.deaf.store(on, Ordering::Relaxed);
    }
}

/// A [`LoopbackSocket`] with a fixed one-way delay on its sends and the two switches.
struct Wire {
    inner: Arc<Mutex<LoopbackSocket>>,
    clock: Clock,
    held: Vec<(u64, Vec<u8>, SocketAddr)>,
    sw: Switches,
}

impl Wire {
    fn new(socket: LoopbackSocket, clock: &Clock) -> (Self, Switches) {
        let sw = Switches::default();
        (
            Self {
                inner: Arc::new(Mutex::new(socket)),
                clock: clock.clone(),
                held: Vec::new(),
                sw: sw.clone(),
            },
            sw,
        )
    }

    fn release_due(&mut self) -> io::Result<()> {
        let now = self.clock.now();
        let mut inner = self.inner.lock().expect("socket");
        // In send order: the delay is the same for every datagram.
        while self.held.first().is_some_and(|(at, _, _)| *at <= now) {
            let (_, bytes, dest) = self.held.remove(0);
            inner.send_to(&bytes, dest)?;
        }
        Ok(())
    }
}

impl DatagramSocket for Wire {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.release_due()?;
        if self.sw.mute.load(Ordering::Relaxed) {
            return Ok(());
        }
        if self.sw.instant.load(Ordering::Relaxed) {
            return self.inner.lock().expect("socket").send_to(bytes, dest);
        }
        self.held
            .push((self.clock.now() + DELAY_MS, bytes.to_vec(), dest));
        Ok(())
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.release_due()?;
        let mut inner = self.inner.lock().expect("socket");
        loop {
            let got = inner.recv_from(buf)?;
            if got.is_some() && self.sw.deaf.load(Ordering::Relaxed) {
                continue;
            }
            return Ok(got);
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.lock().expect("socket").local_addr()
    }
}

fn manifest() -> ContentManifest {
    let base = vec![ManifestAsset {
        id: "levels/arena.level.ron".into(),
        kind: "level".into(),
        content_hash: 11,
    }];
    ContentManifest::build("0.5.0", "1.0.0", 1, 0xabc, &[("base", &base)])
}

fn host_opts() -> HostOptions {
    HostOptions {
        level: "levels/arena.level.ron".into(),
        ..HostOptions::default()
    }
}

/// Every link sealed, loopback included: only a sealed accept carries a resumption credential.
fn sealed() -> SessionSettings {
    SessionSettings {
        encryption: CryptoMode::Required,
        ..SessionSettings::default()
    }
}

/// One scenario: a server session and its clients on one virtual clock.
struct World {
    net: LoopbackNet,
    clock: Clock,
    server: NetSession,
    server_addr: SocketAddr,
    server_events: Vec<ServerSessionEvent>,
}

impl World {
    fn new(settings: SessionSettings) -> Self {
        let net = LoopbackNet::new();
        let clock = Clock::default();
        let server_addr: SocketAddr = "127.0.0.1:7710".parse().expect("addr");
        let (socket, _) = Wire::new(net.bind(server_addr).expect("bind"), &clock);
        let mut server = NetSession::new(settings, manifest());
        server.set_manual_clock(0);
        server.host_on(socket, host_opts(), true).expect("host");
        Self {
            net,
            clock,
            server,
            server_addr,
            server_events: Vec::new(),
        }
    }

    /// A client socket on a fresh port.
    fn socket(&mut self) -> (Wire, Switches) {
        let bound = self.net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
        Wire::new(bound, &self.clock)
    }

    /// One frame: every client, then the server.
    fn step(&mut self, clients: &mut [&mut NetSession]) {
        let now = self.clock.advance(STEP_MS);
        for c in clients.iter_mut() {
            c.set_manual_clock(now);
            c.update();
        }
        self.server.set_manual_clock(now);
        self.server.update();
        self.server_events.extend(self.server.take_server_events());
    }

    fn run_for(&mut self, clients: &mut [&mut NetSession], ms: u64) {
        let end = self.clock.now() + ms;
        while self.clock.now() < end {
            self.step(clients);
        }
    }

    /// Steps until the client is in game or `limit_ms` passed; the time it took.
    fn until_in_game(&mut self, c: &mut NetSession, limit_ms: u64) -> Option<u64> {
        let start = self.clock.now();
        while self.clock.now() < start + limit_ms {
            self.step(&mut [c]);
            if c.state() == ClientSessionState::InGame {
                return Some(self.clock.now() - start);
            }
        }
        None
    }

    fn core(&self) -> &SessionServer {
        self.server.server().expect("server role")
    }

    fn server_totals(&self) -> EndpointTotals {
        self.server.transport_totals().expect("the host's transport")
    }

    /// Joins one client and returns it in game with its switches and `PeerId`.
    fn joined(&mut self, settings: SessionSettings) -> (NetSession, Switches, PeerId) {
        let (socket, sw) = self.socket();
        let mut c = NetSession::new(settings, manifest());
        c.set_manual_clock(self.clock.now());
        c.connect_on(socket, self.server_addr, ConnectOptions::default())
            .expect("connect");
        assert!(
            self.until_in_game(&mut c, 30_000).is_some(),
            "the client reached in_game ({:?})",
            c.state()
        );
        // The client's InGame acknowledgement reaches the server.
        self.run_for(&mut [&mut c], 200);
        let peer = welcomed_peer(&c.take_client_events()).expect("welcomed");
        assert_eq!(self.core().peer_state(peer), Some(PeerState::InGame));
        (c, sw, peer)
    }
}

fn welcomed_peer(events: &[ClientSessionEvent]) -> Option<PeerId> {
    events.iter().rev().find_map(|e| match e {
        ClientSessionEvent::Welcomed(w) => Some(w.peer_id),
        _ => None,
    })
}

fn count(events: &[ServerSessionEvent], f: impl Fn(&ServerSessionEvent) -> bool) -> usize {
    events.iter().filter(|e| f(e)).count()
}

fn resumes(events: &[ServerSessionEvent], peer: PeerId) -> usize {
    count(
        events,
        |e| matches!(e, ServerSessionEvent::PlayerResumed { peer: p } if *p == peer),
    )
}

fn drops(events: &[ServerSessionEvent]) -> usize {
    count(events, |e| matches!(e, ServerSessionEvent::PlayerDropped { .. }))
}

fn leaves(events: &[ServerSessionEvent]) -> usize {
    count(events, |e| matches!(e, ServerSessionEvent::PlayerLeft { .. }))
}

/// A two-way outage longer than the host's timeout, then the path comes back: the client's
/// automatic reconnect ends it. Returns the client's events from the cut on.
fn outage(w: &mut World, c: &mut NetSession, sw: &Switches, peer: PeerId) -> Vec<ClientSessionEvent> {
    let _ = c.take_client_events();
    w.server_events.clear();
    sw.cut(true);
    let mut events = Vec::new();
    let end = w.clock.now() + 30_000;
    while w.clock.now() < end {
        w.step(&mut [c]);
        events.extend(c.take_client_events());
    }
    assert_eq!(
        w.core().peer_state(peer),
        Some(PeerState::Grace),
        "the host's 20 s timeout put the player in grace"
    );
    assert!(c.is_reconnecting(), "reconnecting by itself: {events:?}");
    sw.cut(false);
    assert!(
        w.until_in_game(c, 12_000).is_some(),
        "back in game: {:?}",
        c.state()
    );
    w.run_for(&mut [c], 500);
    events.extend(c.take_client_events());
    events
}

/// The outage path. Before this part the host's transport kept no ticket (`dormant` 0) and the
/// client presented none, so the reconnect was a full handshake: `resumed_one_rtt` stayed 0.
#[test]
fn an_outage_ends_with_a_one_round_trip_transport_resume_of_the_same_player() {
    let mut w = World::new(sealed());
    let (mut c, sw, peer) = w.joined(sealed());
    w.run_for(&mut [&mut c], 2_000);
    let events = outage(&mut w, &mut c, &sw, peer);
    let server = w.server_totals();
    assert_eq!(
        (server.dormant, server.resumed_one_rtt, server.resume_fallbacks),
        (1, 1, 0),
        "the host held the player's ticket and resumed it in one round trip: {server:?}"
    );
    let client = c.transport_totals().expect("the client's transport");
    assert_eq!(
        (
            client.resume_inits,
            client.resumed_one_rtt,
            client.resume_fallbacks
        ),
        (1, 1, 0),
        "the attempt that connected opened with a resume flight and it was admitted: {client:?}"
    );
    assert_eq!(welcomed_peer(&events), Some(peer), "the same PeerId");
    assert_eq!(resumes(&w.server_events, peer), 1, "{:?}", w.server_events);
    assert_eq!(leaves(&w.server_events), 0, "the player never left");
    assert_eq!(w.core().peer_ids(), vec![peer], "no second peer");
    assert_eq!(w.core().peer_state(peer), Some(PeerState::InGame));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                ClientSessionEvent::Disconnected { .. } | ClientSessionEvent::ReconnectFailed { .. }
            ))
            .count(),
        0,
        "{events:?}"
    );

    // A second outage presents the credential of the accept the first resume carried.
    w.run_for(&mut [&mut c], 2_000);
    let events = outage(&mut w, &mut c, &sw, peer);
    let server = w.server_totals();
    assert_eq!(
        (server.dormant, server.resumed_one_rtt, server.resume_fallbacks),
        (2, 2, 0),
        "{server:?}"
    );
    assert_eq!(welcomed_peer(&events), Some(peer), "the same PeerId again");
    assert_eq!(w.core().peer_ids(), vec![peer]);
}

/// The early return: the client's upstream dies, the game reconnects from a new socket two
/// seconds later, long before the host's 20 s timeout. The transport resumes the connection in
/// place (a takeover, one round trip) and the session resumes the player on the same link with
/// no drop and no second slot, exactly as a takeover from a new link does.
#[test]
fn an_early_return_takes_the_live_connection_over_in_one_round_trip() {
    let mut w = World::new(sealed());
    let (mut c, old, peer) = w.joined(sealed());
    w.run_for(&mut [&mut c], 2_000);
    let slots = w.core().players_in_use();
    let link = w.core().link_of(peer).expect("a link");
    w.server_events.clear();
    old.mute.store(true, Ordering::Relaxed);
    w.run_for(&mut [&mut c], 2_000);
    assert_eq!(w.core().peer_state(peer), Some(PeerState::InGame));
    c.reset();
    let (socket, _new) = w.socket();
    c.set_manual_clock(w.clock.now());
    c.connect_on(
        socket,
        w.server_addr,
        ConnectOptions {
            resume: true,
            ..ConnectOptions::default()
        },
    )
    .expect("reconnect");
    assert!(
        w.until_in_game(&mut c, 15_000).is_some(),
        "the reconnect reached in_game ({:?})",
        c.state()
    );
    w.run_for(&mut [&mut c], 200);
    let server = w.server_totals();
    assert_eq!(
        (
            server.resumed_one_rtt,
            server.resume_takeovers,
            server.resume_fallbacks,
            server.dormant
        ),
        (1, 1, 0, 0),
        "a one-round-trip takeover of the live connection: {server:?}"
    );
    assert_eq!(
        welcomed_peer(&c.take_client_events()),
        Some(peer),
        "the same PeerId"
    );
    assert_eq!(resumes(&w.server_events, peer), 1, "{:?}", w.server_events);
    assert_eq!(
        drops(&w.server_events),
        0,
        "a takeover, not a drop: {:?}",
        w.server_events
    );
    assert_eq!(leaves(&w.server_events), 0, "the player never left");
    assert_eq!(w.core().peer_ids(), vec![peer], "no second peer");
    assert_eq!(w.core().players_in_use(), slots, "slot count unchanged");
    assert_eq!(
        w.core().link_of(peer),
        Some(link),
        "the transport kept the peer's id, so the session's link is the same"
    );
    // The resumed link works both ways: the player stays in game past the old link's timeout.
    w.run_for(&mut [&mut c], 25_000);
    assert_eq!(c.state(), ClientSessionState::InGame);
    assert_eq!(w.core().peer_state(peer), Some(PeerState::InGame));
    assert_eq!(leaves(&w.server_events) + drops(&w.server_events), 0);
}

/// The fallback spec 120 § 10.3 names: a plain link (loopback under `CryptoMode::Auto`) has no
/// PSK and gets no credential. Its reconnect presents the 16-byte ticket in the full handshake
/// and still comes back as the same transport peer and the same player.
#[test]
fn a_plain_session_link_comes_back_through_the_full_handshake_with_its_ticket() {
    let mut w = World::new(SessionSettings::default());
    let (mut c, sw, peer) = w.joined(SessionSettings::default());
    assert!(!c.link_sealed(), "loopback under Auto is plain");
    w.run_for(&mut [&mut c], 2_000);
    let link = w.core().link_of(peer).expect("a link");
    let events = outage(&mut w, &mut c, &sw, peer);
    let server = w.server_totals();
    assert_eq!(
        (
            server.dormant,
            server.resumes,
            server.resumed_one_rtt,
            server.resume_inits
        ),
        (1, 1, 0, 0),
        "the ticket resumed the dormant transport peer through the full handshake: {server:?}"
    );
    assert_eq!(welcomed_peer(&events), Some(peer), "the same PeerId");
    assert_eq!(w.core().link_of(peer), Some(link), "the same transport peer");
    assert_eq!(resumes(&w.server_events, peer), 1);
    assert_eq!(leaves(&w.server_events), 0, "the player never left");
    assert_eq!(w.core().peer_ids(), vec![peer]);
}

/// A host with no reconnect grace keeps no session slot, so its transport keeps no ticket
/// either: a timeout is the player's end, as before.
#[test]
fn a_host_without_a_reconnect_grace_keeps_no_transport_hold() {
    let settings = SessionSettings {
        reconnect_grace_s: 0.0,
        ..sealed()
    };
    let mut w = World::new(settings.clone());
    let (mut c, sw, peer) = w.joined(settings);
    w.server_events.clear();
    sw.cut(true);
    w.run_for(&mut [&mut c], 25_000);
    assert_eq!(w.server_totals().dormant, 0);
    assert_eq!(w.core().peer_state(peer), None, "the player is gone");
    assert_eq!(leaves(&w.server_events), 1);
}

/// A suspend (an Android pause): the client says goodbye with `suspended`, the session keeps the
/// player in grace and closes the link itself. That close holds the transport's ticket as a
/// timeout would (`NetServer::kick_held`), so `resume_on` comes back in one round trip as the
/// same player.
#[test]
fn a_suspended_client_resumes_in_one_round_trip() {
    let mut w = World::new(sealed());
    let (mut c, old, peer) = w.joined(sealed());
    w.run_for(&mut [&mut c], 1_000);
    w.server_events.clear();
    old.instant.store(true, Ordering::Relaxed);
    c.suspend();
    // Frames stop on the client; the goodbye reaches the host, which closes the link.
    w.run_for(&mut [], 1_000);
    assert_eq!(w.core().peer_state(peer), Some(PeerState::Grace));
    assert_eq!(w.core().link_of(peer), None);
    let (socket, _new) = w.socket();
    c.set_manual_clock(w.clock.now());
    assert!(
        c.resume_on(socket).expect("resume"),
        "a suspended session resumes"
    );
    assert!(
        w.until_in_game(&mut c, 15_000).is_some(),
        "the resume reached in_game ({:?})",
        c.state()
    );
    w.run_for(&mut [&mut c], 200);
    let server = w.server_totals();
    assert_eq!(
        (server.resumed_one_rtt, server.resume_fallbacks),
        (1, 0),
        "the suspended player's link came back in one round trip: {server:?}"
    );
    assert_eq!(
        welcomed_peer(&c.take_client_events()),
        Some(peer),
        "the same PeerId"
    );
    assert_eq!(resumes(&w.server_events, peer), 1, "{:?}", w.server_events);
    assert_eq!(leaves(&w.server_events), 0, "the player never left");
    assert_eq!(w.core().peer_ids(), vec![peer]);
}

// --- The session core, link by link -----------------------------------------------------------

fn hello(token: Option<[u8; 16]>) -> Vec<u8> {
    Hello {
        protocol_version: 1,
        protocol_hash: 0xabc,
        manifest: manifest(),
        player_names: vec!["ada".into()],
        password: None,
        auth: None,
        client_rate_bytes: 0,
        reconnect_token: token,
    }
    .encode()
    .expect("hello")
}

fn core() -> SessionServer {
    let mut s = SessionServer::new(SessionSettings::default(), host_opts(), manifest(), [7; 32]);
    s.set_dev_build(false);
    s
}

fn welcome_token(s: &mut SessionServer, link: LinkId) -> [u8; 16] {
    s.take_outgoing()
        .into_iter()
        .filter(|o| o.link == link)
        .find_map(|o| match SessionMsg::decode(&o.bytes) {
            Ok(SessionMsg::Welcome(w)) => Some(w.reconnect_token),
            _ => None,
        })
        .expect("a welcome on the link")
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Live {
    Loading,
    Joining,
    InGame,
}

const A: &str = "10.0.0.1:5000";
const B: &str = "10.0.0.2:6000";

/// A core with one player on link 1 in `state`; its token and id.
fn one_player(state: Live) -> (SessionServer, [u8; 16], PeerId) {
    let mut s = core();
    s.link_up(LinkId(1), A.parse().expect("addr"), 0);
    s.receive(LinkId(1), &hello(None), 0);
    let token = welcome_token(&mut s, LinkId(1));
    let peer = s.peer_of(LinkId(1)).expect("peer");
    if state != Live::Loading {
        let ready = SessionMsg::LevelReady {
            travel_id: s.travel_id(),
        }
        .encode()
        .expect("msg");
        s.receive(LinkId(1), &ready, 0);
    }
    if state == Live::InGame {
        let ig = SessionMsg::InGame { tick: 0 }.encode().expect("msg");
        s.receive(LinkId(1), &ig, 0);
    }
    while s.poll_event().is_some() {}
    let _ = s.take_outgoing();
    (s, token, peer)
}

fn events(s: &mut SessionServer) -> Vec<ServerSessionEvent> {
    std::iter::from_fn(|| s.poll_event()).collect()
}

/// The transport resumed link 1 in place: the player is detached and keeps its state, the link
/// waits for a hello, and the hello with the player's token resumes it on the same link. No
/// drop, no leave, no second slot, and the link is not closed (a takeover from a new link
/// closes the old one; here the old one is the new one).
#[test]
fn a_link_resumed_in_place_waits_for_the_hello_and_resumes_the_player() {
    for state in [Live::Loading, Live::Joining, Live::InGame] {
        let (mut s, token, peer) = one_player(state);
        let before = (s.peer_state(peer), s.players_in_use());
        s.link_up(LinkId(1), B.parse().expect("addr"), 100);
        assert_eq!(s.peer_of(LinkId(1)), None, "{state:?}: detached");
        assert_eq!(
            s.peer_behind(LinkId(1)),
            Some(peer),
            "{state:?}: waiting for the link"
        );
        assert_eq!(s.link_of(peer), None, "{state:?}");
        assert_eq!(s.handshaking(), 1, "{state:?}: the link is pending");
        assert_eq!(
            (s.peer_state(peer), s.players_in_use()),
            before,
            "{state:?}: the player keeps its state and slot"
        );
        s.receive(LinkId(1), &hello(Some(token)), 100);
        let ev = events(&mut s);
        assert_eq!(resumes(&ev, peer), 1, "{state:?}: {ev:?}");
        assert_eq!(drops(&ev) + leaves(&ev), 0, "{state:?}: {ev:?}");
        assert_eq!(s.link_of(peer), Some(LinkId(1)), "{state:?}");
        assert_eq!(s.peer_behind(LinkId(1)), Some(peer), "{state:?}");
        assert_eq!(s.peer_ids(), vec![peer], "{state:?}: no second peer");
        assert_eq!(s.players_in_use(), before.1, "{state:?}");
        assert_eq!(s.peer_addr(peer), Some(B.parse().expect("addr")), "{state:?}");
        assert!(
            s.take_quiet_closes().is_empty() && s.take_closes().is_empty(),
            "{state:?}: the resumed link stays open"
        );
        // The welcome rotates the token, as on every resume.
        assert_ne!(welcome_token(&mut s, LinkId(1)), token, "{state:?}");
    }
}

/// The resumed link goes down before any hello: the player lost its link the way a timeout
/// loses it. In game it enters grace and its token still resumes it from a new link.
#[test]
fn a_resumed_link_that_goes_down_unheard_is_the_loss_of_the_old_link() {
    let (mut s, token, peer) = one_player(Live::InGame);
    s.link_up(LinkId(1), B.parse().expect("addr"), 100);
    s.link_down(LinkId(1), DisconnectReason::Timeout, 200);
    let ev = events(&mut s);
    assert_eq!(s.peer_state(peer), Some(PeerState::Grace), "{ev:?}");
    assert_eq!((drops(&ev), leaves(&ev)), (1, 0), "{ev:?}");
    assert_eq!(s.peer_behind(LinkId(1)), None);
    assert_eq!(s.handshaking(), 0);
    s.link_up(LinkId(2), B.parse().expect("addr"), 300);
    s.receive(LinkId(2), &hello(Some(token)), 300);
    let ev = events(&mut s);
    assert_eq!(resumes(&ev, peer), 1, "{ev:?}");
    assert_eq!(s.link_of(peer), Some(LinkId(2)));

    // A player still loading its first join has no grace: it leaves, as on any lost link.
    let (mut s, _, peer) = one_player(Live::Loading);
    s.link_up(LinkId(1), B.parse().expect("addr"), 100);
    s.link_down(LinkId(1), DisconnectReason::Timeout, 200);
    let ev = events(&mut s);
    assert_eq!(s.peer_state(peer), None, "{ev:?}");
    assert_eq!(leaves(&ev), 1, "{ev:?}");
}

/// The resumed link says nothing: the handshake timeout refuses it and the old player enters
/// grace then, not at its own 20 s silence timeout.
#[test]
fn a_resumed_link_that_sends_no_hello_times_out_as_a_handshake() {
    let (mut s, _, peer) = one_player(Live::InGame);
    s.link_up(LinkId(1), B.parse().expect("addr"), 100);
    let hs = (SessionSettings::default().handshake_timeout_s * 1000.0) as u64;
    s.update(100 + hs - 1);
    assert_eq!(s.peer_state(peer), Some(PeerState::InGame), "still waiting");
    s.update(100 + hs);
    let ev = events(&mut s);
    assert_eq!(s.peer_state(peer), Some(PeerState::Grace), "{ev:?}");
    assert_eq!((drops(&ev), leaves(&ev)), (1, 0), "{ev:?}");
    assert!(
        ev.iter()
            .any(|e| matches!(e, ServerSessionEvent::Refused { link, .. } if *link == LinkId(1))),
        "{ev:?}"
    );
    assert_eq!(s.peer_behind(LinkId(1)), None);
}

/// Another player's hello on the resumed link (no token, or a token that matches nothing) is a
/// new player there; the old one lost its link and waits in grace for its own token.
#[test]
fn another_hello_on_a_resumed_link_is_a_new_player_and_the_old_one_enters_grace() {
    let (mut s, token, peer) = one_player(Live::InGame);
    s.link_up(LinkId(1), B.parse().expect("addr"), 100);
    s.receive(LinkId(1), &hello(None), 100);
    let ev = events(&mut s);
    let newcomer = s.peer_of(LinkId(1)).expect("a player on the link");
    assert_ne!(newcomer, peer);
    assert_eq!(s.peer_state(peer), Some(PeerState::Grace), "{ev:?}");
    assert_eq!((drops(&ev), leaves(&ev), resumes(&ev, peer)), (1, 0, 0), "{ev:?}");
    assert_eq!(s.link_of(peer), None);
    assert_eq!(s.peer_ids().len(), 2);
    // The old player's token still finds its slot, from another link.
    s.link_up(LinkId(2), A.parse().expect("addr"), 200);
    s.receive(LinkId(2), &hello(Some(token)), 200);
    let ev = events(&mut s);
    assert_eq!(resumes(&ev, peer), 1, "{ev:?}");
    assert_eq!(s.link_of(peer), Some(LinkId(2)));
    assert_eq!(s.peer_of(LinkId(1)), Some(newcomer));
}

/// A message queued for the old connection never reaches the one that replaced it under the
/// same link: the new connection's first session message is its welcome.
#[test]
fn nothing_queued_for_the_old_connection_reaches_the_resumed_link() {
    let (mut s, token, peer) = one_player(Live::InGame);
    s.send_game(peer, vec![1, 2, 3]);
    s.link_up(LinkId(1), B.parse().expect("addr"), 100);
    assert!(
        s.take_outgoing().iter().all(|o| o.link != LinkId(1)),
        "the old connection's queue is dropped with it"
    );
    s.receive(LinkId(1), &hello(Some(token)), 100);
    let out = s.take_outgoing();
    let first = out.iter().find(|o| o.link == LinkId(1)).expect("a message");
    assert!(
        matches!(SessionMsg::decode(&first.bytes), Ok(SessionMsg::Welcome(_))),
        "the welcome comes first"
    );
}

// --- The transport ------------------------------------------------------------------------------

#[derive(Debug)]
struct Game;

impl NetGame for Game {
    type Message = u32;
    type Input = u32;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.session_resume_one_rtt";
    const PROTOCOL_VERSION: u32 = 1;
}

type Server = NetServer<Game, LoopbackSocket>;
type Client = NetClient<Game, LoopbackSocket>;

/// A transport server that keeps tickets and one connected client: its id and ticket.
struct Transport {
    net: LoopbackNet,
    server: Server,
    server_addr: SocketAddr,
    now: u64,
    peer: PeerId,
    ticket: [u8; 16],
}

impl Transport {
    fn new(port: u16) -> Self {
        let net = LoopbackNet::new();
        let server_addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut server: Server = NetServer::with_secret(
            net.bind(server_addr).expect("bind"),
            ServerConfig {
                resume_grace_ms: 60_000,
                ..ServerConfig::default()
            },
            [9; 32],
        );
        let mut client = Self::connect(&net, server_addr, None);
        let mut now = 0;
        let mut joined = None;
        while joined.is_none() && now < 5_000 {
            now += 10;
            client.update(now);
            server.update(now);
            while let Some(e) = server.poll_event() {
                if let ServerEvent::PeerJoined { peer, .. } = e {
                    joined = Some(peer);
                }
            }
        }
        let peer = joined.expect("joined");
        now += 10;
        client.update(now);
        let ticket = client.resume_ticket().expect("a ticket");
        // The client is dropped here: it goes silent.
        Self {
            net,
            server,
            server_addr,
            now,
            peer,
            ticket,
        }
    }

    fn connect(net: &LoopbackNet, server_addr: SocketAddr, resume: Option<[u8; 16]>) -> Client {
        let socket = net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let config = ClientConfig {
            resume,
            ..ClientConfig::default()
        };
        NetClient::connect(socket, server_addr, config, &()).expect("connect")
    }

    /// The server's events until `limit_ms` passed or `stop` held for one.
    fn events_until(
        &mut self,
        limit_ms: u64,
        stop: impl Fn(&ServerEvent<Game>) -> bool,
    ) -> Vec<ServerEvent<Game>> {
        let end = self.now + limit_ms;
        let mut seen = Vec::new();
        while self.now < end {
            self.now += 100;
            self.server.update(self.now);
            let mut done = false;
            while let Some(e) = self.server.poll_event() {
                done |= stop(&e);
                seen.push(e);
            }
            if done {
                break;
            }
        }
        seen
    }

    /// A client that presents the first client's ticket: `("joined" | "resumed", id)`.
    fn come_back(&mut self) -> Vec<(&'static str, PeerId)> {
        let mut back = Self::connect(&self.net, self.server_addr, Some(self.ticket));
        let mut seen = Vec::new();
        let end = self.now + 5_000;
        while seen.is_empty() && self.now < end {
            self.now += 10;
            back.update(self.now);
            self.server.update(self.now);
            while let Some(e) = self.server.poll_event() {
                match e {
                    ServerEvent::PeerJoined { peer, .. } => seen.push(("joined", peer)),
                    ServerEvent::PeerResumed { peer, .. } => seen.push(("resumed", peer)),
                    _ => {}
                }
            }
        }
        seen
    }
}

/// `forget_dormant` ends a hold at once: the id and slot are free, the leave is reported once,
/// the natural-expiry counter does not move, and the forgotten ticket resumes nothing (its
/// holder joins as a new peer).
#[test]
fn forget_dormant_ends_the_hold_and_its_ticket() {
    let mut t = Transport::new(7720);
    let peer = t.peer;
    // The client is silent; the server's timeout makes the peer dormant.
    let seen = t.events_until(40_000, |e| matches!(e, ServerEvent::PeerDormant { .. }));
    assert!(
        seen.iter()
            .any(|e| matches!(e, ServerEvent::PeerDormant { peer: p, .. } if *p == peer)),
        "the silent peer went dormant"
    );
    assert_eq!(t.server.dormant_peers().count(), 1);
    assert!(t.server.forget_dormant(peer));
    assert!(!t.server.forget_dormant(peer), "nothing left to forget");
    assert_eq!(t.server.dormant_peers().count(), 0);
    let left = std::iter::from_fn(|| t.server.poll_event())
        .filter(|e| matches!(e, ServerEvent::PeerLeft { peer: p, .. } if *p == peer))
        .count();
    assert_eq!(left, 1, "one leave for the forgotten hold");
    assert_eq!(t.server.totals().dormant_expired, 0, "not a natural expiry");
    let seen = t.come_back();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, "joined", "{seen:?}");
    assert_ne!(
        seen[0].1, peer,
        "a new id: the old one was released, never resumed"
    );
}

/// `kick_held` removes the connection as a kick does and holds the peer as a timeout does: one
/// `PeerDormant`, no `PeerLeft`, and the ticket resumes the same peer. `kick` holds nothing.
#[test]
fn kick_held_keeps_the_peer_dormant_and_its_ticket_good() {
    let mut t = Transport::new(7721);
    let peer = t.peer;
    assert!(
        t.server
            .kick_held(peer, regia_net::handshake::DisconnectReason::Kicked)
    );
    let seen: Vec<_> = std::iter::from_fn(|| t.server.poll_event()).collect();
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e, ServerEvent::PeerDormant { peer: p, .. } if *p == peer))
            .count(),
        1,
        "held"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, ServerEvent::PeerLeft { .. })),
        "not left"
    );
    assert_eq!(t.server.dormant_peers().count(), 1);
    assert_eq!(t.come_back(), vec![("resumed", peer)]);
    assert_eq!(t.server.dormant_peers().count(), 0);

    let mut t = Transport::new(7722);
    let peer = t.peer;
    assert!(
        t.server
            .kick(peer, regia_net::handshake::DisconnectReason::Kicked)
    );
    assert_eq!(t.server.dormant_peers().count(), 0, "a plain kick holds nothing");
    let seen = t.come_back();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, "joined", "{seen:?}");
}
