// SPDX-License-Identifier: MPL-2.0
//! Hostile input over real UDP loopback (spec 70 § hostile input, ADR-7892): an input bundle whose
//! codec reports an over-cap field disconnects only its sender at once; other undecodable bundles,
//! including trailing bytes after the bundle, score malformed points that a record's credit no
//! longer cancels, so a stream of them drops the sender; a valid peer in the same session keeps
//! playing. Time is a step counter (10 ms per step); a pump reads the socket until every datagram
//! the other side sent has arrived, so nothing sleeps.

use std::net::SocketAddr;

use regia_net::prelude::*;

/// The command tail cap of [`Cmd`]: a decoder sees a longer tail as `WireError::TooLong`.
const TAIL_CAP: usize = 16;
const STEP_MS: u64 = 10;
/// Socket reads one pump may spend waiting for in-flight loopback datagrams.
const PUMP_SPINS: usize = 200_000;

/// A test input with a capped command tail, the shape of a game's tick sample.
#[derive(Debug, Clone, PartialEq)]
enum Cmd {
    /// A well-formed sample.
    Valid(Vec<u8>),
    /// Encodes a tail of this many bytes, past [`TAIL_CAP`] (a hostile client).
    OverCap(u16),
    /// Encodes an unknown tag (a hostile client).
    BadTag,
    /// Encodes a valid empty sample followed by one stray byte (a hostile client).
    Trailing,
}

impl NetCodec for Cmd {
    fn encode(&self, w: &mut WireWriter) {
        match self {
            Cmd::Valid(tail) => {
                w.u8(0);
                w.u16(tail.len() as u16);
                w.raw(tail);
            }
            Cmd::OverCap(n) => {
                w.u8(0);
                w.u16(*n);
                w.raw(&vec![0xAB; usize::from(*n)]);
            }
            Cmd::BadTag => w.u8(9),
            Cmd::Trailing => {
                w.u8(0);
                w.u16(0);
                w.u8(0xEE);
            }
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Cmd::Valid(r.bytes_u16(TAIL_CAP)?.to_vec())),
            _ => Err(WireError::Invalid("cmd tag")),
        }
    }
}

#[derive(Debug)]
struct Game;

impl NetGame for Game {
    type Message = u32;
    type Input = Cmd;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.input_rejection";
    const PROTOCOL_VERSION: u32 = 1;
}

type Server = NetServer<Game, UdpTransport>;
type Client = NetClient<Game, UdpTransport>;

fn localhost() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// A server, one honest client and one hostile client (input redundancy 1, so every record carries
/// exactly the newest input), connected. `None` when the host has no loopback UDP.
struct Session {
    server: Server,
    good: Client,
    bad: Client,
    good_peer: PeerId,
    bad_peer: PeerId,
    now: u64,
    events: Vec<ServerEvent<Game>>,
}

impl Session {
    fn start() -> Option<Self> {
        let Ok(socket) = UdpTransport::bind(localhost()) else {
            println!("SKIP no loopback UDP");
            return None;
        };
        let server: Server = NetServer::with_secret(socket, ServerConfig::default(), [5; 32]);
        let server_addr = server.local_addr().expect("server bound");
        let connect = |name: &str, redundancy: usize| {
            let config = ClientConfig {
                name: name.into(),
                input_redundancy: redundancy,
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
        let good = connect("good", 3);
        let bad = connect("bad", 1);
        let mut s = Self {
            server,
            good,
            bad,
            good_peer: PeerId(0),
            bad_peer: PeerId(0),
            now: 0,
            events: Vec::new(),
        };
        for _ in 0..300 {
            if s.good.is_connected() && s.bad.is_connected() {
                break;
            }
            s.step();
        }
        assert!(
            s.good.is_connected() && s.bad.is_connected(),
            "both clients connect over loopback UDP"
        );
        s.good_peer = s.good.peer_id().expect("good peer id");
        s.bad_peer = s.bad.peer_id().expect("bad peer id");
        s.events.clear();
        Some(s)
    }

    /// Reads the server socket until it has every datagram the clients handed to theirs.
    fn pump_server(&mut self) {
        let sent = self.good.totals().datagrams_out + self.bad.totals().datagrams_out;
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

    /// Reads the client sockets until they hold what the server sent (best effort: a client the
    /// server dropped may stop counting).
    fn pump_clients(&mut self) {
        let sent = self.server.totals().datagrams_out;
        for _ in 0..PUMP_SPINS {
            self.good.receive(self.now);
            self.bad.receive(self.now);
            if self.good.totals().datagrams_in + self.bad.totals().datagrams_in >= sent {
                break;
            }
            std::thread::yield_now();
        }
        while self.good.poll_event().is_some() {}
        while self.bad.poll_event().is_some() {}
    }

    /// One step: the clients send, the server reads, the server sends, the clients read.
    fn step(&mut self) {
        self.now += STEP_MS;
        self.good.flush(self.now);
        self.bad.flush(self.now);
        self.pump_server();
        self.server.flush(self.now);
        self.pump_clients();
    }

    /// One tick: the honest client sends a valid input for the server's current tick and the
    /// hostile one sends `hostile` (if any); then the step runs and the server advances the tick.
    /// Returns the inputs the server used for that tick.
    fn tick(&mut self, hostile: Option<Cmd>) -> TickInputs<Cmd> {
        let tick = self.server.driver().tick();
        self.good
            .send_input(tick, Cmd::Valid(vec![tick as u8]))
            .expect("good input queued");
        if let Some(cmd) = hostile
            && self.bad.is_connected()
        {
            self.bad.send_input(tick, cmd).expect("hostile input queued");
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
}

/// One over-cap input record disconnects its sender with `ProtocolError` at once; the honest peer
/// keeps its connection and its input for the same tick.
#[test]
fn over_cap_input_disconnects_only_its_sender_at_once() {
    let Some(mut s) = Session::start() else {
        return;
    };
    for _ in 0..5 {
        let inputs = s.tick(Some(Cmd::Valid(vec![1, 2, 3])));
        assert_eq!(
            inputs.len(),
            2,
            "both peers' inputs arrive before the hostile one"
        );
    }
    let malformed_before = s.server.totals().malformed;
    let tick = s.server.driver().tick();
    let inputs = s.tick(Some(Cmd::OverCap(64)));
    assert_eq!(
        s.server.totals().malformed,
        malformed_before + 1,
        "exactly one over-cap record reached the server"
    );
    assert_eq!(
        s.left(s.bad_peer),
        Some(DisconnectReason::ProtocolError),
        "one over-cap input disconnects its sender at once (events: {:?})",
        s.events
    );
    assert_eq!(
        inputs.get(&s.good_peer),
        Some(&Cmd::Valid(vec![tick as u8])),
        "the honest peer's input for the same tick is used"
    );
    assert!(!inputs.contains_key(&s.bad_peer));
    assert_eq!(s.left(s.good_peer), None, "no one else is dropped");
    for _ in 0..5 {
        let inputs = s.tick(None);
        assert!(inputs.contains_key(&s.good_peer));
    }
    assert!(s.good.is_connected(), "the honest client stays connected");
    assert_eq!(
        s.server.session().peer(s.good_peer).map(|p| p.role),
        Some(PeerRole::Player)
    );
    assert!(s.server.session().peer(s.bad_peer).is_none());
    assert!(!s.bad.is_connected(), "the hostile client heard the disconnect");
}

/// Records that each carry one undecodable input cross the malformed limit and drop the sender
/// with `MalformedPacket`; the honest peer never misses a tick. Each one scores
/// `MALFORMED_WEIGHT` (16) points (ADR-8606), so the 1,024-point limit is crossed after 64 records
/// plus the few the 4-point-a-second leak takes back over their ticks.
#[test]
fn malformed_input_records_cross_the_limit_and_drop_the_sender() {
    let Some(mut s) = Session::start() else {
        return;
    };
    let mut sent = 0;
    while sent < 1_100 && s.left(s.bad_peer).is_none() {
        let inputs = s.tick(Some(Cmd::BadTag));
        sent += 1;
        assert!(
            inputs.contains_key(&s.good_peer),
            "honest input arrives at tick {}",
            s.server.driver().tick() - 1
        );
        assert!(!inputs.contains_key(&s.bad_peer));
    }
    assert_eq!(
        s.left(s.bad_peer),
        Some(DisconnectReason::MalformedPacket),
        "{sent} malformed input records did not drop the sender (server malformed total {})",
        s.server.totals().malformed
    );
    let floor = regia_net::server::MALFORMED_SCORE_LIMIT / regia_net::server::MALFORMED_WEIGHT;
    assert!(
        (floor..=floor + 8).contains(&sent),
        "1,024 points at 16 a record: {floor} records and the leak's few more drop a peer (dropped after {sent})"
    );
    assert_eq!(s.left(s.good_peer), None, "no one else is dropped");
    assert_eq!(s.server.driver().missed(s.good_peer), 0);
    assert!(s.good.is_connected());
}

/// Bytes after the last input of a bundle are an error: the bundle is refused and scored, not used.
#[test]
fn trailing_bytes_after_an_input_bundle_are_refused() {
    let Some(mut s) = Session::start() else {
        return;
    };
    let _ = s.tick(Some(Cmd::Valid(vec![])));
    let malformed_before = s.server.totals().malformed;
    let inputs = s.tick(Some(Cmd::Trailing));
    assert!(
        !inputs.contains_key(&s.bad_peer),
        "a bundle with trailing bytes is not used: {inputs:?}"
    );
    assert_eq!(s.server.totals().malformed, malformed_before + 1);
    assert!(inputs.contains_key(&s.good_peer));
    assert_eq!(
        s.left(s.bad_peer),
        None,
        "trailing bytes score points; they are not fatal"
    );
}

/// The rejections a session's events carry, in order, as `(peer, kind)`.
fn rejections(events: &[ServerEvent<Game>]) -> Vec<(PeerId, RejectKind)> {
    events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::Rejected { peer, kind, .. } => Some((*peer, *kind)),
            _ => None,
        })
        .collect()
}

/// `ServerEvent::Rejected` names the sender and the kind, comes before the `PeerLeft` of a fatal
/// rejection, and is never raised for the honest peer.
#[test]
fn rejected_event_names_the_peer_and_the_kind() {
    let Some(mut s) = Session::start() else {
        return;
    };
    let _ = s.tick(Some(Cmd::BadTag));
    let _ = s.tick(Some(Cmd::Trailing));
    let _ = s.tick(Some(Cmd::Valid(vec![4])));
    let _ = s.tick(Some(Cmd::OverCap(17)));
    let _ = s.tick(None);
    assert_eq!(
        rejections(&s.events),
        vec![
            (s.bad_peer, RejectKind::InputMalformed),
            (s.bad_peer, RejectKind::InputMalformed),
            (s.bad_peer, RejectKind::InputTooLong),
        ]
    );
    let errors: Vec<&WireError> = s
        .events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::Rejected { error, .. } => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors[0], &WireError::Invalid("cmd tag"));
    assert_eq!(errors[1], &WireError::Trailing(1));
    assert_eq!(
        errors[2],
        &WireError::TooLong {
            len: 17,
            max: TAIL_CAP
        }
    );
    let fatal = s
        .events
        .iter()
        .position(|e| {
            matches!(
                e,
                ServerEvent::Rejected {
                    kind: RejectKind::InputTooLong,
                    ..
                }
            )
        })
        .expect("fatal rejection");
    assert!(
        matches!(
            s.events.get(fatal + 1),
            Some(ServerEvent::PeerLeft { peer, reason: DisconnectReason::ProtocolError }) if *peer == s.bad_peer
        ),
        "PeerLeft follows the fatal rejection: {:?}",
        s.events
    );
    assert!(RejectKind::InputTooLong.is_fatal());
    assert!(!RejectKind::InputMalformed.is_fatal());
    assert_eq!(RejectKind::InputTooLong.as_str(), "input_too_long");
    assert!(s.good.is_connected());
}

/// A host may act on the first rejection itself: kicking the sender on `Rejected` removes only it.
#[test]
fn host_can_kick_on_the_first_rejection() {
    let Some(mut s) = Session::start() else {
        return;
    };
    let _ = s.tick(Some(Cmd::BadTag));
    let offenders: Vec<PeerId> = rejections(&s.events).into_iter().map(|(p, _)| p).collect();
    assert_eq!(offenders, vec![s.bad_peer]);
    for peer in offenders {
        assert!(s.server.kick(peer, DisconnectReason::Kicked));
    }
    let _ = s.tick(None);
    assert_eq!(s.left(s.bad_peer), Some(DisconnectReason::Kicked));
    assert_eq!(s.left(s.good_peer), None);
    assert!(!s.bad.is_connected());
    assert!(s.good.is_connected());
}
