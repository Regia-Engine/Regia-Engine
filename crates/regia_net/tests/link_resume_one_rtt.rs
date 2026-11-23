// SPDX-License-Identifier: MPL-2.0
//! `NetLink` presents the resumption credential on every reconnect path (wire 6, part 3;
//! ADR-10088, spec 120 § 10.3).
//!
//! Parts 1 and 2 built the one-round-trip resume in `NetClient` and `NetServer` (ADR-8858,
//! ADR-8902); until this part the session layer never presented the credential, so every
//! `NetLink` reconnect was the full handshake of three round trips. Here a sealed link on the
//! in-memory network, manual clocks (no wall clock, no sleeps):
//!
//! - [`NetLink::resume_on`] (the game's own resume, a phone back from a new address): one
//!   `ResumeInit`, one `ResumeReply`, the same `PeerId`.
//! - The automatic reconnect of ADR-8352, on the endpoint's own socket and on fresh sockets from a
//!   [`LinkSocketOpener`] (the relay and Steam shape): every attempt of the outage presents the
//!   credential with a newer flight, and the attempt the server hears resumes in one round trip.
//! - A second outage presents the credential of the accept the first resume carried, never the
//!   spent one.
//! - The fallback the spec names stays: a plain link holds no credential and comes back through
//!   the full handshake with its ticket.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use regia_app::App;
use regia_net::bridge::NetFrame;
use regia_net::crypto::CryptoMode;
use regia_net::link::{LinkClock, LinkEvent, LinkOptions, LinkSocketOpener, NetLink, NetLinkPlugin};
use regia_net::resume::NetResumeState;
use regia_net::runtime::NetNode;
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};
use regia_net::status::{EndpointTotals, NetAgentState};
use regia_net::{NetGame, NetMode, NetPlugin, PeerId};

#[derive(Debug)]
struct OneRttGame;

impl NetGame for OneRttGame {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.link_resume_one_rtt";
    const PROTOCOL_VERSION: u32 = 1;
}

/// One app update of 60 Hz.
const STEP_US: u64 = 16_667;
const FRAMES_PER_S: u64 = 60;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn options(crypto: CryptoMode) -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: STEP_US,
        },
        crypto,
        ..LinkOptions::default()
    }
}

fn app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app
}

/// A socket whose path can be cut both ways, counting the datagrams the client sends while the
/// path is up.
struct GateSocket {
    socket: LoopbackSocket,
    down: Arc<AtomicBool>,
    sent: Arc<AtomicU32>,
}

impl DatagramSocket for GateSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if self.down.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.sent.fetch_add(1, Ordering::Relaxed);
        self.socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.socket.recv_from(buf)?;
            if got.is_none() || !self.down.load(Ordering::Relaxed) {
                return Ok(got);
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

struct Rig {
    net: LoopbackNet,
    server: App,
    a: App,
    down: Arc<AtomicBool>,
    sent: Arc<AtomicU32>,
    a_events: Vec<LinkEvent>,
    server_events: Vec<LinkEvent>,
}

impl Rig {
    /// A server and one client on a gated socket; `opener`: the client connects through a
    /// [`LinkSocketOpener`], so each automatic reconnect attempt opens a fresh socket.
    fn new(port: u16, crypto: CryptoMode, opener: bool) -> Self {
        let net = LoopbackNet::new();
        let mut server = app(NetMode::DedicatedServer);
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<OneRttGame>(Box::new(net.bind(addr(port)).expect("bind")), &options(crypto))
            .expect("host");
        let down = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU32::new(0));
        let mut a = app(NetMode::Client);
        {
            let mut link = a.world_mut().resource_mut::<NetLink>();
            if opener {
                let (net, down, sent) = (net.clone(), down.clone(), sent.clone());
                let open: LinkSocketOpener = Arc::new(move || {
                    Ok(Box::new(GateSocket {
                        socket: net.bind(addr(0))?,
                        down: down.clone(),
                        sent: sent.clone(),
                    }))
                });
                link.connect_via::<OneRttGame>(open, addr(port), &options(crypto))
                    .expect("connect");
            } else {
                link.connect::<OneRttGame>(
                    Box::new(GateSocket {
                        socket: net.bind(addr(0)).expect("bind"),
                        down: down.clone(),
                        sent: sent.clone(),
                    }),
                    addr(port),
                    &options(crypto),
                )
                .expect("connect");
            }
        }
        let mut rig = Self {
            net,
            server,
            a,
            down,
            sent,
            a_events: Vec::new(),
            server_events: Vec::new(),
        };
        rig.run(3 * FRAMES_PER_S);
        assert!(rig.a.world().resource::<NetLink>().is_connected(), "a connected");
        rig
    }

    fn step(&mut self) {
        self.server.update();
        let events = self.server.world_mut().resource_mut::<NetLink>().take_events();
        self.server_events.extend(events);
        self.a.update();
        let events = self.a.world_mut().resource_mut::<NetLink>().take_events();
        self.a_events.extend(events);
    }

    fn run(&mut self, frames: u64) {
        for _ in 0..frames {
            self.step();
        }
    }

    fn a_peer(&self) -> Option<PeerId> {
        let link = self.a.world().resource::<NetLink>();
        link.status()
            .filter(|s| s.state == "connected")
            .and_then(|s| s.peer_id)
            .map(PeerId)
    }

    /// Steps until A is connected as `peer` again; the frames it took.
    fn until_back(&mut self, peer: PeerId, limit: u64) -> Option<u64> {
        for n in 0..limit {
            if self.a_peer() == Some(peer) {
                return Some(n);
            }
            self.step();
        }
        None
    }

    fn server_totals(&self) -> EndpointTotals {
        self.server.world().resource::<NetAgentState>().reports()["server"]
            .status
            .totals
    }

    fn client_totals(&self) -> EndpointTotals {
        self.a.world().resource::<NetAgentState>().reports()["client"]
            .status
            .totals
    }

    /// A comes back from a new address (a new socket), the game's own `resume_on`.
    fn resume_a_from_new_address(&mut self) {
        let socket = GateSocket {
            socket: self.net.bind(addr(0)).expect("bind"),
            down: self.down.clone(),
            sent: self.sent.clone(),
        };
        let resumed = self
            .a
            .world_mut()
            .resource_mut::<NetLink>()
            .resume_on(Box::new(socket))
            .expect("resume");
        assert!(resumed, "a was a client");
    }
}

/// The game's own resume after an 11 s outage, from a new port of the same address block: the
/// server still holds the old connection (inside the 20 s timeout) and the resume takes it over.
/// One `ResumeInit` and one `ResumeReply`: the client is connected, as the same peer, after
/// sending one datagram, where the full handshake sends three (request, cookie echo, proof).
#[test]
fn resume_on_presents_the_credential_and_is_one_round_trip() {
    let mut rig = Rig::new(7961, CryptoMode::Required, false);
    let peer = rig.a_peer().expect("peer");
    assert!(rig.a.world().resource::<NetLink>().is_sealed(), "a sealed link");
    assert!(rig.a.world().resource::<NetLink>().resume_ticket().is_some());
    rig.down.store(true, Ordering::Relaxed);
    rig.run(11 * FRAMES_PER_S);
    rig.down.store(false, Ordering::Relaxed);
    let sent_before = rig.sent.load(Ordering::Relaxed);
    rig.resume_a_from_new_address();
    // The frames until the client is connected again, and the datagrams it sent to get there.
    let mut sent_to_connect = None;
    for _ in 0..3 * FRAMES_PER_S {
        rig.step();
        if sent_to_connect.is_none() && rig.a_peer() == Some(peer) {
            sent_to_connect = Some(rig.sent.load(Ordering::Relaxed) - sent_before);
        }
    }
    let server = rig.server_totals();
    let client = rig.client_totals();
    assert_eq!(
        (
            server.resumed_one_rtt,
            client.resumed_one_rtt,
            client.resume_inits
        ),
        (1, 1, 1),
        "the resume was one round trip: server {server:?}; client {client:?}"
    );
    assert_eq!(
        (server.resume_fallbacks, client.resume_fallbacks),
        (0, 0),
        "nothing fell back to the full handshake"
    );
    assert_eq!(rig.a_peer(), Some(peer), "the same PeerId");
    let sent = sent_to_connect.expect("a came back as the same peer");
    // One flight, then whatever the frame that saw the reply sent after it (the confirming
    // record): never the three datagrams of a full handshake before the accept.
    assert!(sent <= 2, "connected after {sent} datagrams");
    assert_eq!(
        rig.server.world().resource::<NetNode>().peers(),
        vec![peer],
        "no second peer for a"
    );
    assert!(
        !rig.server_events
            .iter()
            .any(|e| matches!(e, LinkEvent::Left { .. })),
        "a never left: {:?}",
        rig.server_events
    );
    let st = rig.server.world().resource::<NetResumeState>();
    assert_eq!((st.takeovers, st.resumed, st.went_dormant), (1, 0, 0));
}

/// The automatic reconnect of ADR-8352: 45 s of two-way loss, 25 s past the timeout. Every attempt
/// during the outage presents the credential (each a newer flight, none heard); the attempt after
/// the link returns is one `ResumeInit` answered with one `ResumeReply`.
fn the_automatic_reconnect_is_one_round_trip(port: u16, opener: bool) {
    let mut rig = Rig::new(port, CryptoMode::Required, opener);
    let peer = rig.a_peer().expect("peer");
    rig.down.store(true, Ordering::Relaxed);
    rig.run(45 * FRAMES_PER_S);
    assert!(
        rig.a.world().resource::<NetLink>().is_reconnecting(),
        "a reconnects by itself after the timeout"
    );
    rig.down.store(false, Ordering::Relaxed);
    let frames = rig
        .until_back(peer, 10 * FRAMES_PER_S)
        .expect("a came back as the same peer");
    assert!(
        frames <= 2 * FRAMES_PER_S,
        "back within 2 s, took {frames} frames"
    );
    rig.run(2 * FRAMES_PER_S);
    let server = rig.server_totals();
    assert_eq!(
        (server.resumed_one_rtt, server.resume_fallbacks),
        (1, 0),
        "the server resumed a in one round trip: {server:?}"
    );
    assert_eq!(
        rig.client_totals().resumed_one_rtt,
        1,
        "the attempt that connected finished on a ResumeReply"
    );
    let st = rig.server.world().resource::<NetResumeState>();
    assert_eq!(
        (st.went_dormant, st.resumed),
        (1, 1),
        "dormant, then the same peer"
    );
    assert_eq!(rig.server.world().resource::<NetNode>().peers(), vec![peer]);
    assert!(
        !rig.a_events
            .iter()
            .any(|e| matches!(e, LinkEvent::Ended { .. } | LinkEvent::ReconnectFailed { .. })),
        "the outage did not end the link: {:?}",
        rig.a_events
    );
}

#[test]
fn the_automatic_reconnect_on_its_own_socket_is_one_round_trip() {
    the_automatic_reconnect_is_one_round_trip(7962, false);
}

#[test]
fn the_automatic_reconnect_on_fresh_sockets_is_one_round_trip() {
    the_automatic_reconnect_is_one_round_trip(7963, true);
}

/// Two outages in a row: the second resume presents the credential of the accept the first
/// resume carried. A ticket is single use per epoch (ADR-8902), so presenting the first one again
/// would be refused (`resume_epoch`) and fall back to three round trips.
#[test]
fn a_second_outage_presents_the_credential_of_the_first_resume() {
    let mut rig = Rig::new(7964, CryptoMode::Required, false);
    let peer = rig.a_peer().expect("peer");
    for round in 1..=2u64 {
        rig.down.store(true, Ordering::Relaxed);
        rig.run(11 * FRAMES_PER_S);
        rig.down.store(false, Ordering::Relaxed);
        rig.resume_a_from_new_address();
        rig.run(3 * FRAMES_PER_S);
        assert_eq!(rig.a_peer(), Some(peer), "round {round}: the same PeerId");
        let server = rig.server_totals();
        assert_eq!(
            (server.resumed_one_rtt, server.resume_fallbacks),
            (round, 0),
            "round {round}: one round trip, no fallback: {server:?}; refusals {:?}",
            rig.server.world().resource::<NetAgentState>().reports()["server"]
                .status
                .resume_refusals
        );
    }
}

/// The fallback the spec names: a plain link has no PSK, so its accept carries no resumption
/// credential, and the reconnect is the full handshake that presents the 16-byte ticket. The
/// player still keeps its `PeerId`.
#[test]
fn a_plain_link_reconnects_through_the_full_handshake() {
    let mut rig = Rig::new(7965, CryptoMode::Off, false);
    let peer = rig.a_peer().expect("peer");
    assert!(!rig.a.world().resource::<NetLink>().is_sealed());
    rig.down.store(true, Ordering::Relaxed);
    rig.run(11 * FRAMES_PER_S);
    rig.down.store(false, Ordering::Relaxed);
    rig.resume_a_from_new_address();
    rig.run(3 * FRAMES_PER_S);
    assert_eq!(rig.a_peer(), Some(peer), "the same PeerId");
    let (server, client) = (rig.server_totals(), rig.client_totals());
    assert_eq!(
        (client.resume_inits, server.resumed_one_rtt),
        (0, 0),
        "no credential, no resume flight: server {server:?}; client {client:?}"
    );
    let st = rig.server.world().resource::<NetResumeState>();
    assert_eq!((st.takeovers, st.resumed), (1, 0));
}
