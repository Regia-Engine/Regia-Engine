// SPDX-License-Identifier: MPL-2.0
//! Swallowed decode errors at the endpoints (spec 120 § 11.2, ADR-7984), over real UDP loopback
//! with encryption required on both sides: input from a spectator is refused, scored and named
//! (`ServerEvent::Rejected` with `spectator_input`) instead of dropped for free; a server message
//! that fails to decode on the ordered reliable stream ends the client's link with a protocol
//! error instead of silently putting it out of step, while an unreliable one is dropped and
//! counted. A valid peer in the same session is never affected. Time is a step counter (10 ms per
//! step); a pump reads the sockets until every datagram the other side sent has arrived, so nothing
//! sleeps.

use std::net::SocketAddr;

use regia_net::crypto::CryptoMode;
use regia_net::prelude::*;

const STEP_MS: u64 = 10;
/// Socket reads one pump may spend waiting for in-flight loopback datagrams.
const PUMP_SPINS: usize = 200_000;
/// Longest command tail [`Cmd`] decodes: large enough that a big input travels on the reliable
/// input lane.
const TAIL_CAP: usize = 4096;

/// A test input with a command tail.
#[derive(Debug, Clone, PartialEq)]
struct Cmd(Vec<u8>);

impl NetCodec for Cmd {
    fn encode(&self, w: &mut WireWriter) {
        w.u16(self.0.len() as u16);
        w.raw(&self.0);
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Cmd(r.bytes_u16(TAIL_CAP)?.to_vec()))
    }
}

/// A server message; `Bad` encodes a tag the decoder refuses (a broken or hostile server).
#[derive(Debug, Clone, PartialEq)]
enum Msg {
    Good(u32),
    Bad,
}

impl NetCodec for Msg {
    fn encode(&self, w: &mut WireWriter) {
        match self {
            Msg::Good(n) => {
                w.u8(0);
                w.u32(*n);
            }
            Msg::Bad => w.u8(7),
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Msg::Good(r.u32()?)),
            _ => Err(WireError::Invalid("msg tag")),
        }
    }
}

#[derive(Debug)]
struct Game;

impl NetGame for Game {
    type Message = Msg;
    type Input = Cmd;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.swallowed_decode";
    const PROTOCOL_VERSION: u32 = 1;
}

type Server = NetServer<Game, UdpTransport>;
type Client = NetClient<Game, UdpTransport>;

fn localhost() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// A server and two clients (`a`, `b`) on sealed links. `None` when the host has no loopback UDP.
struct Session {
    server: Server,
    a: Client,
    b: Client,
    a_peer: PeerId,
    b_peer: PeerId,
    now: u64,
    events: Vec<ServerEvent<Game>>,
    a_events: Vec<ClientEvent<Game>>,
    b_events: Vec<ClientEvent<Game>>,
}

impl Session {
    fn start(b_role: PeerRole) -> Option<Self> {
        let Ok(socket) = UdpTransport::bind(localhost()) else {
            println!("SKIP no loopback UDP");
            return None;
        };
        let config = ServerConfig {
            crypto: CryptoMode::Required,
            ..ServerConfig::default()
        };
        let server: Server = NetServer::with_secret(socket, config, [9; 32]);
        let server_addr = server.local_addr().expect("server bound");
        let connect = |name: &str, role: PeerRole| {
            let config = ClientConfig {
                name: name.into(),
                role,
                crypto: CryptoMode::Required,
                ..ClientConfig::default()
            };
            NetClient::connect(
                UdpTransport::bind(localhost()).expect("bind client"),
                server_addr,
                config,
                &(),
            )
            .expect("join fits")
        };
        let a = connect("a", PeerRole::Player);
        let b = connect("b", b_role);
        let mut s = Self {
            server,
            a,
            b,
            a_peer: PeerId(0),
            b_peer: PeerId(0),
            now: 0,
            events: Vec::new(),
            a_events: Vec::new(),
            b_events: Vec::new(),
        };
        for _ in 0..300 {
            if s.a.is_connected() && s.b.is_connected() {
                break;
            }
            s.step();
        }
        assert!(
            s.a.is_connected() && s.b.is_connected(),
            "both clients connect over loopback UDP"
        );
        s.a_peer = s.a.peer_id().expect("a peer id");
        s.b_peer = s.b.peer_id().expect("b peer id");
        assert!(s.a.is_sealed() && s.b.is_sealed(), "both links are encrypted");
        assert!(s.server.peer_sealed(s.a_peer) && s.server.peer_sealed(s.b_peer));
        s.events.clear();
        s.a_events.clear();
        s.b_events.clear();
        Some(s)
    }

    /// Reads the server socket until it has every datagram the clients handed to theirs.
    fn pump_server(&mut self) {
        let sent = self.a.totals().datagrams_out + self.b.totals().datagrams_out;
        for _ in 0..PUMP_SPINS {
            self.server.receive(self.now);
            if self.server.totals().datagrams_in >= sent {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            self.server.totals().datagrams_in >= sent,
            "loopback delivered {} of {} client datagrams",
            self.server.totals().datagrams_in,
            sent
        );
        self.events
            .extend(std::iter::from_fn(|| self.server.poll_event()));
    }

    /// Reads the client sockets until they hold what the server sent (best effort: a client that
    /// ended stops counting).
    fn pump_clients(&mut self) {
        let sent = self.server.totals().datagrams_out;
        for _ in 0..PUMP_SPINS {
            self.a.receive(self.now);
            self.b.receive(self.now);
            if self.a.totals().datagrams_in + self.b.totals().datagrams_in >= sent {
                break;
            }
            std::thread::yield_now();
        }
        self.a_events.extend(std::iter::from_fn(|| self.a.poll_event()));
        self.b_events.extend(std::iter::from_fn(|| self.b.poll_event()));
    }

    fn step(&mut self) {
        self.now += STEP_MS;
        self.a.flush(self.now);
        self.b.flush(self.now);
        self.pump_server();
        self.server.flush(self.now);
        self.pump_clients();
    }

    /// One tick: `a` sends a valid input, `b` sends `b_input` (if any and still connected).
    fn tick(&mut self, b_input: Option<Cmd>) -> TickInputs<Cmd> {
        let tick = self.server.driver().tick();
        self.a
            .send_input(tick, Cmd(vec![tick as u8]))
            .expect("a input queued");
        if let Some(cmd) = b_input
            && self.b.is_connected()
        {
            self.b.send_input(tick, cmd).expect("b input queued");
        }
        self.step();
        self.server.advance_tick(|_, _| {})
    }

    fn left(&self, peer: PeerId) -> Option<DisconnectReason> {
        self.events.iter().find_map(|e| match e {
            ServerEvent::PeerLeft { peer: p, reason } if *p == peer => Some(*reason),
            _ => None,
        })
    }

    /// `(peer, kind name)` of every rejection so far, in order.
    fn rejections(&self) -> Vec<(PeerId, &'static str)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ServerEvent::Rejected { peer, kind, .. } => Some((*peer, kind.as_str())),
                _ => None,
            })
            .collect()
    }

    fn ended(events: &[ClientEvent<Game>]) -> Option<ClientEnd> {
        events.iter().find_map(|e| match e {
            ClientEvent::Ended(end) => Some(end.clone()),
            _ => None,
        })
    }

    fn messages(events: &[ClientEvent<Game>]) -> Vec<(Channel, Msg)> {
        events
            .iter()
            .filter_map(|e| match e {
                ClientEvent::Message { channel, message } => Some((*channel, message.clone())),
                _ => None,
            })
            .collect()
    }
}

/// Item 1: a spectator's input bundle is refused, scored one point and named, never dropped for
/// free; the player in the same session keeps its input every tick.
#[test]
fn spectator_input_is_refused_scored_and_named() {
    let Some(mut s) = Session::start(PeerRole::Spectator) else {
        return;
    };
    let malformed_before = s.server.totals().malformed;
    let inputs = s.tick(Some(Cmd(vec![1, 2, 3])));
    assert_eq!(
        s.rejections(),
        vec![(s.b_peer, "spectator_input")],
        "the spectator's input is refused and names its sender (events: {:?})",
        s.events
    );
    assert_eq!(
        s.server.totals().malformed,
        malformed_before + 1,
        "the refusal is scored"
    );
    assert!(inputs.contains_key(&s.a_peer), "the player's input is used");
    assert!(!inputs.contains_key(&s.b_peer));
    assert_eq!(s.left(s.b_peer), None, "spectator input scores; it is not fatal");
    assert_eq!(s.left(s.a_peer), None);
}

/// Item 1, reliable lane: an input too large for a datagram travels on the reliable input lane;
/// from a spectator it is refused the same way.
#[test]
fn spectator_reliable_input_is_refused_and_named() {
    let Some(mut s) = Session::start(PeerRole::Spectator) else {
        return;
    };
    let _ = s.tick(Some(Cmd(vec![0x5A; 2_000])));
    let _ = s.tick(None);
    assert!(
        s.b.totals().reliable_inputs >= 1,
        "the big input went on the reliable lane"
    );
    let refused: Vec<_> = s.rejections();
    assert!(
        !refused.is_empty() && refused.iter().all(|r| *r == (s.b_peer, "spectator_input")),
        "every refusal names the spectator: {refused:?}"
    );
    assert_eq!(
        s.server.totals().reliable_inputs,
        0,
        "no spectator input reached the tick driver"
    );
    assert!(s.a.is_connected() && s.b.is_connected());
}

/// Item 1, the score: records that each carry one spectator input cross the malformed limit and
/// drop the spectator with `MalformedPacket`; the player never misses a tick.
#[test]
fn spectator_input_records_cross_the_limit_and_drop_the_spectator() {
    let Some(mut s) = Session::start(PeerRole::Spectator) else {
        return;
    };
    let mut sent = 0;
    while sent < 1_100 && s.left(s.b_peer).is_none() {
        let inputs = s.tick(Some(Cmd(vec![sent as u8])));
        sent += 1;
        assert!(inputs.contains_key(&s.a_peer));
    }
    assert_eq!(
        s.left(s.b_peer),
        Some(DisconnectReason::MalformedPacket),
        "{sent} spectator input records did not drop the spectator (server malformed {})",
        s.server.totals().malformed
    );
    assert!(
        sent >= 1_024,
        "no fewer than 1024 points drop a peer (after {sent})"
    );
    assert_eq!(s.left(s.a_peer), None, "no one else is dropped");
    assert_eq!(s.server.driver().missed(s.a_peer), 0);
    assert!(s.a.is_connected());
}

/// Item 3: a server message that fails to decode on the reliable stream ends that client's link
/// with a protocol error, and the server hears the goodbye; the other client, sent a valid message
/// in the same flush, keeps its link and its message.
#[test]
fn a_reliable_decode_error_ends_the_client_link_with_a_protocol_error() {
    let Some(mut s) = Session::start(PeerRole::Player) else {
        return;
    };
    let malformed_before = s.a.totals().malformed;
    s.server
        .send(s.a_peer, Channel::GAME_RPC, &Msg::Bad)
        .expect("queued");
    s.server
        .send(s.b_peer, Channel::GAME_RPC, &Msg::Good(7))
        .expect("queued");
    for _ in 0..3 {
        s.step();
    }
    assert_eq!(
        Session::ended(&s.a_events),
        Some(ClientEnd::Disconnected(DisconnectReason::ProtocolError)),
        "a reliable decode error is a protocol error, never a silent drop (a events: {:?})",
        s.a_events
    );
    assert!(!s.a.is_connected());
    assert_eq!(s.a.totals().malformed, malformed_before + 1, "and it is counted");
    assert!(
        s.left(s.a_peer).is_some(),
        "the client told the server it left (events: {:?})",
        s.events
    );
    assert!(s.server.session().peer(s.a_peer).is_none());
    assert_eq!(
        Session::messages(&s.b_events),
        vec![(Channel::GAME_RPC, Msg::Good(7))]
    );
    assert!(s.b.is_connected(), "the other client is unaffected");
    assert_eq!(Session::ended(&s.b_events), None);
}

/// Item 3, unreliable: a message that fails to decode on the unreliable channel is dropped and
/// counted; the link stays up and the next message arrives.
#[test]
fn an_unreliable_decode_error_is_dropped_and_counted() {
    let Some(mut s) = Session::start(PeerRole::Player) else {
        return;
    };
    let malformed_before = s.a.totals().malformed;
    s.server
        .send(s.a_peer, Channel::Unreliable, &Msg::Bad)
        .expect("queued");
    s.step();
    s.server
        .send(s.a_peer, Channel::GAME_RPC, &Msg::Good(1))
        .expect("queued");
    for _ in 0..3 {
        s.step();
    }
    assert_eq!(s.a.totals().malformed, malformed_before + 1);
    assert!(s.a.is_connected(), "an unreliable decode error is not fatal");
    assert_eq!(Session::ended(&s.a_events), None);
    assert_eq!(
        Session::messages(&s.a_events),
        vec![(Channel::GAME_RPC, Msg::Good(1))]
    );
    assert!(s.b.is_connected());
}
