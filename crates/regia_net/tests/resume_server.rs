// SPDX-License-Identifier: MPL-2.0
//! Wire 6, part 2 (ADR-8902; ADR-8733 (3) as amended by ADR-8858): the server's admission of a
//! one-round-trip resume.
//!
//! A `ResumeInit` admits nobody. It leaves a pending resume and gets a `ResumeReply`; the peer is
//! resumed when the first sealed record under the new keys arrives (key confirmation). These
//! tests drive a real `NetServer` and real `NetClient`s over the loopback network. A client's
//! socket is tapped, so its datagrams can be played again from any address, and it can be made
//! deaf, so a reply is lost. Addresses are non-loopback, so `CryptoMode::Auto` seals. Time is a
//! step counter; nothing sleeps.

mod common;

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use common::{Duel, Team};
use regia_net::crypto::{PENDING_TTL_MS, ResumeCredential, ResumeInit, ResumeRefusal, ResumeRetry};
use regia_net::handshake::{PacketKind, split_datagram};
use regia_net::prelude::*;
use regia_net::status::EndpointTotals;
use regia_net::{LoopbackSocket, NetStatus};

type Server = NetServer<Duel, LoopbackSocket>;
type Client = NetClient<Duel, Tap>;

const STEP_MS: u64 = 10;
const GRACE_MS: u64 = 60_000;

/// A client socket that records what it sends and can stop hearing.
struct Tap {
    socket: LoopbackSocket,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    deaf: Arc<AtomicBool>,
}

impl DatagramSocket for Tap {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.sent.lock().expect("tap").push(bytes.to_vec());
        self.socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.socket.recv_from(buf)?;
            if got.is_none() || !self.deaf.load(Ordering::Relaxed) {
                return Ok(got);
            }
        }
    }

    fn max_datagram_size(&self, dest: SocketAddr) -> usize {
        self.socket.max_datagram_size(dest)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// The test's hold on a client's socket.
#[derive(Clone)]
struct Wire {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    deaf: Arc<AtomicBool>,
}

impl Wire {
    fn deafen(&self, deaf: bool) {
        self.deaf.store(deaf, Ordering::Relaxed);
    }

    /// The first `ResumeInit` datagram the client sent.
    fn resume_init(&self) -> Vec<u8> {
        self.sent
            .lock()
            .expect("tap")
            .iter()
            .find(|d| matches!(split_datagram(d), Some((PacketKind::ResumeInit, _))))
            .cloned()
            .expect("the client sent a ResumeInit")
    }

    fn sent(&self) -> usize {
        self.sent.lock().expect("tap").len()
    }
}

fn server_addr() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], 7777))
}

/// An address in the block every first join comes from.
fn home(port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 2], port))
}

/// An address in another block.
fn away(port: u16) -> SocketAddr {
    SocketAddr::from(([10, 9, 0, 9], port))
}

fn server(net: &LoopbackNet, edit: impl FnOnce(&mut ServerConfig)) -> Server {
    let mut config = ServerConfig {
        resume_grace_ms: GRACE_MS,
        ..ServerConfig::default()
    };
    edit(&mut config);
    NetServer::with_secret(net.bind(server_addr()).expect("bind server"), config, [7; 32])
}

fn client(net: &LoopbackNet, addr: SocketAddr, edit: impl FnOnce(&mut ClientConfig)) -> (Client, Wire) {
    let mut config = ClientConfig {
        name: "ada".into(),
        ..ClientConfig::default()
    };
    edit(&mut config);
    let wire = Wire {
        sent: Arc::default(),
        deaf: Arc::default(),
    };
    let socket = Tap {
        socket: net.bind(addr).expect("bind client"),
        sent: wire.sent.clone(),
        deaf: wire.deaf.clone(),
    };
    let client = NetClient::connect(socket, server_addr(), config, &Team(1)).expect("join fits");
    (client, wire)
}

/// A client that presents `from`'s resume ticket and its resumption credential.
fn returning(net: &LoopbackNet, addr: SocketAddr, from: &Client) -> (Client, Wire) {
    let (ticket, credential) = (from.resume_ticket(), from.resumption());
    assert!(credential.is_some(), "a sealed accept carries a credential");
    client(net, addr, |config| {
        config.resume = ticket;
        config.resumption = credential;
    })
}

/// A client that presents a credential taken earlier.
fn returning_with(
    net: &LoopbackNet,
    addr: SocketAddr,
    ticket: Option<[u8; 16]>,
    credential: &ResumeCredential,
) -> (Client, Wire) {
    let credential = credential.clone();
    client(net, addr, |config| {
        config.resume = ticket;
        config.resumption = Some(credential);
    })
}

#[derive(Default)]
struct Seen {
    joined: Vec<PeerId>,
    resumed: Vec<PeerId>,
    left: Vec<PeerId>,
    dormant: Vec<PeerId>,
}

struct Rig {
    net: LoopbackNet,
    s: Server,
    now: u64,
    seen: Seen,
}

impl Rig {
    fn new(edit: impl FnOnce(&mut ServerConfig)) -> Self {
        let net = LoopbackNet::new();
        let s = server(&net, edit);
        Self {
            net,
            s,
            now: 0,
            seen: Seen::default(),
        }
    }

    fn events(&mut self) {
        while let Some(event) = self.s.poll_event() {
            match event {
                ServerEvent::PeerJoined { peer, .. } => self.seen.joined.push(peer),
                ServerEvent::PeerResumed { peer, .. } => self.seen.resumed.push(peer),
                ServerEvent::PeerLeft { peer, .. } => self.seen.left.push(peer),
                ServerEvent::PeerDormant { peer, .. } => self.seen.dormant.push(peer),
                _ => {}
            }
        }
    }

    /// One step of the clock: every client, then the server.
    fn step(&mut self, clients: &mut [&mut Client]) {
        self.now += STEP_MS;
        for c in clients.iter_mut() {
            c.update(self.now);
        }
        self.s.update(self.now);
        self.events();
    }

    fn steps(&mut self, n: usize, clients: &mut [&mut Client]) {
        for _ in 0..n {
            self.step(clients);
        }
    }

    /// The server alone: it reads what is on its socket and answers.
    fn serve(&mut self) {
        self.now += STEP_MS;
        self.s.update(self.now);
        self.events();
    }

    /// Steps until `c` is connected or has ended. Returns the datagrams it had sent by then.
    fn connect(&mut self, c: &mut Client, wire: &Wire) -> usize {
        for _ in 0..600 {
            self.now += STEP_MS;
            c.receive(self.now);
            if c.is_connected() || matches!(c.state(), ClientState::Ended(_)) {
                let sent = wire.sent();
                c.flush(self.now);
                self.s.update(self.now);
                self.events();
                return sent;
            }
            c.flush(self.now);
            self.s.update(self.now);
            self.events();
        }
        wire.sent()
    }

    /// A first join from `addr`, settled so its accept is confirmed.
    fn joined(&mut self, addr: SocketAddr) -> (Client, Wire, PeerId) {
        let (mut c, wire) = client(&self.net, addr, |_| {});
        self.connect(&mut c, &wire);
        assert!(c.is_connected() && c.is_sealed(), "{:?}", c.state());
        self.steps(5, &mut [&mut c]);
        let peer = c.peer_id().expect("peer");
        (c, wire, peer)
    }

    /// Lets the clock run with nobody talking until `peer` is dormant.
    fn until_dormant(&mut self, peer: PeerId) {
        for _ in 0..600 {
            self.now += 100;
            self.s.update(self.now);
            self.events();
            if self.s.dormant_peers().any(|(held, _)| held == peer) {
                return;
            }
        }
        panic!("peer {peer} never went dormant");
    }

    fn totals(&self) -> EndpointTotals {
        self.s.totals()
    }

    fn peer_addr(&self, peer: PeerId) -> Option<SocketAddr> {
        self.s.session().peer(peer).map(|p| p.addr)
    }

    /// Sends raw bytes to the server from `addr` and returns what came back to that address.
    fn play(&mut self, addr: SocketAddr, datagram: &[u8]) -> Vec<Vec<u8>> {
        let mut socket = self.net.bind(addr).expect("bind");
        socket.send_to(datagram, server_addr()).expect("send");
        self.serve();
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        while let Some((n, _)) = socket.recv_from(&mut buf).expect("receive") {
            out.push(buf[..n].to_vec());
        }
        out
    }
}

fn kind_of(datagram: &[u8]) -> Option<PacketKind> {
    split_datagram(datagram).map(|(kind, _)| kind)
}

fn decode_init(datagram: &[u8]) -> ResumeInit {
    let (kind, body) = split_datagram(datagram).expect("a regia_net datagram");
    assert_eq!(kind, PacketKind::ResumeInit);
    ResumeInit::decode(body).expect("decodes")
}

/// The whole path, step by step. The flight alone changes nothing on the server: the peer keeps
/// its old connection and address, no event is raised, and the ticket it presented stays good.
/// The client is connected after one datagram. Its first sealed record is what resumes the peer.
#[test]
fn a_resume_is_one_round_trip_and_admits_only_at_key_confirmation() {
    let mut rig = Rig::new(|_| {});
    let (first, _, peer) = rig.joined(home(5000));
    let before = rig.totals();
    let old_addr = rig.peer_addr(peer).expect("live");

    let (mut back, wire) = returning(&rig.net, home(5001), &first);
    // The flight leaves the client and reaches the server.
    rig.now += STEP_MS;
    back.flush(rig.now);
    assert_eq!(wire.sent(), 1);
    assert_eq!(kind_of(&wire.resume_init()), Some(PacketKind::ResumeInit));
    rig.s.receive(rig.now);
    rig.events();
    assert_eq!(
        rig.s.pending_resumes(),
        1,
        "answered, waiting for the confirmation"
    );
    let t = rig.totals();
    assert_eq!((t.resume_inits, t.resume_fallbacks), (1, 0));
    assert_eq!(t.handshakes, before.handshakes + 1, "one `ee`");
    assert_eq!(
        (t.resumes, t.resume_takeovers, t.resumed_one_rtt, t.accepted),
        (0, 0, 0, before.accepted),
        "nothing is admitted on ResumeInit"
    );
    assert_eq!(rig.peer_addr(peer), Some(old_addr), "the old connection stands");
    assert!(rig.seen.resumed.is_empty() && rig.seen.joined.len() == 1);
    let epochs = rig.s.resume_epochs(peer).expect("epochs");
    assert_eq!(
        (epochs.confirmed(), epochs.minted()),
        (1, 2),
        "the reply's ticket is epoch 2; the presented one is still good"
    );
    assert_eq!(rig.s.last_resume_refusal(), None);

    // The reply reaches the client: connected, one datagram sent in all.
    back.receive(rig.now);
    assert!(back.is_connected(), "{:?}", back.state());
    assert_eq!(back.peer_id(), Some(peer), "the same peer");
    assert_eq!(wire.sent(), 1, "one round trip");
    assert_eq!(back.totals().resumed_one_rtt, 1);
    assert_eq!(rig.s.pending_resumes(), 1, "still not admitted");

    // The client's first sealed record is the key confirmation.
    rig.now += STEP_MS;
    back.flush(rig.now);
    rig.s.receive(rig.now);
    rig.events();
    assert_eq!(rig.s.pending_resumes(), 0);
    assert_eq!(
        rig.peer_addr(peer),
        Some(home(5001)),
        "the peer moved with its resume"
    );
    assert_eq!(rig.seen.resumed, vec![peer]);
    assert_eq!(rig.seen.joined.len(), 1, "no new peer");
    let t = rig.totals();
    assert_eq!((t.resumes, t.resume_takeovers, t.resumed_one_rtt), (1, 1, 1));
    assert_eq!(t.resume_fallbacks, 0);
    assert_eq!(rig.s.session().len(), 1);
    let epochs = rig.s.resume_epochs(peer).expect("epochs");
    assert_eq!(
        (epochs.confirmed(), epochs.minted()),
        (2, 2),
        "confirmed by that record"
    );

    // The link works both ways afterwards, and the client holds the next credential.
    rig.steps(5, &mut [&mut back]);
    assert!(back.is_connected());
    let next = back
        .resumption()
        .expect("the reply's accept carries a new ticket");
    let claims = rig.s.ticket_keys().open(next.ticket(), rig.now).expect("opens");
    assert_eq!((claims.peer, claims.epoch), (peer.0, 2));
    assert_eq!(Some(claims.resume_id), back.resume_ticket());
}

/// The review's first assert (ADR-8858 A1), for a peer: a `ResumeInit` played again from its own
/// block and from another block installs no peer, takes over nothing, moves no address and
/// leaves the presented ticket good. The real client then resumes with that same ticket.
#[test]
fn a_replayed_resume_init_from_its_block_or_another_admits_nobody() {
    let mut rig = Rig::new(|_| {});
    let (mut first, _, peer) = rig.joined(home(5000));
    let credential = first.resumption().expect("credential");
    let ticket = first.resume_ticket();
    let old_addr = rig.peer_addr(peer).expect("live");

    // The real flight, whose reply is lost: the client never confirms.
    let (mut lost, wire) = returning_with(&rig.net, home(5001), ticket, &credential);
    wire.deafen(true);
    rig.now += STEP_MS;
    lost.flush(rig.now);
    rig.s.update(rig.now);
    let captured = wire.resume_init();
    assert_eq!(rig.s.pending_resumes(), 1);
    let answered = rig.totals();
    drop(lost);

    // The same bytes from the ticket's own block, another port: the stored reply and nothing else.
    for round in 0..5 {
        let got = rig.play(home(6000 + round), &captured);
        assert_eq!(got.len(), 1);
        assert_eq!(kind_of(&got[0]), Some(PacketKind::ResumeReply));
    }
    // From another block: no cookie, so a `ResumeRetry`; with the cookie, the stored reply.
    let got = rig.play(away(6000), &captured);
    assert_eq!(got.len(), 1);
    let (kind, body) = split_datagram(&got[0]).expect("datagram");
    assert_eq!(kind, PacketKind::ResumeRetry);
    assert_eq!(got[0].len(), 29, "no larger than the request");
    let retry = ResumeRetry::decode(body).expect("retry");
    let mut init = decode_init(&captured);
    assert_eq!(retry.client_nonce, init.client_nonce);
    init.cookie = Some(retry.cookie);
    let with_cookie = init.to_datagram().expect("encodes");
    let got = rig.play(away(6000), &with_cookie);
    assert_eq!(got.len(), 1);
    assert_eq!(kind_of(&got[0]), Some(PacketKind::ResumeReply));

    let t = rig.totals();
    assert_eq!(t.resume_replays, 6, "five from the block, one from outside");
    assert_eq!(t.handshakes, answered.handshakes, "no `ee` for a replay");
    assert_eq!((t.resumes, t.resume_takeovers, t.resumed_one_rtt), (0, 0, 0));
    assert_eq!(t.accepted, answered.accepted);
    assert_eq!(rig.s.pending_resumes(), 1, "the one real flight");
    assert_eq!(rig.s.resume_refusals(ResumeRefusal::OtherBlock), 1);
    assert_eq!(rig.peer_addr(peer), Some(old_addr), "no address moved");
    assert!(rig.seen.resumed.is_empty() && rig.seen.left.is_empty());
    assert_eq!(rig.seen.joined, vec![peer], "no peer installed");
    let epochs = rig.s.resume_epochs(peer).expect("epochs");
    assert_eq!(epochs.confirmed(), 1, "nothing was confirmed");
    let claims = rig
        .s
        .ticket_keys()
        .open(credential.ticket(), rig.now)
        .expect("opens");
    assert_eq!(
        epochs.check(claims.epoch),
        Ok(()),
        "the presented ticket is still good"
    );
    // The old connection is alive through all of it.
    rig.steps(5, &mut [&mut first]);
    assert!(first.is_connected());

    // The pending flight lapses unconfirmed; played again it is an old flight: no answer at all.
    rig.now += PENDING_TTL_MS;
    rig.steps(2, &mut [&mut first]);
    assert_eq!(rig.s.pending_resumes(), 0);
    assert_eq!(rig.totals().resume_lapsed, 1);
    assert!(
        rig.play(home(6100), &captured).is_empty(),
        "an old flight gets no answer"
    );
    assert!(rig.play(away(6000), &with_cookie).is_empty());
    assert_eq!(rig.s.resume_refusals(ResumeRefusal::StaleFlight), 2);
    assert_eq!(rig.totals().handshakes, answered.handshakes);
    assert_eq!(rig.peer_addr(peer), Some(old_addr));

    // The ticket was never spent: its holder comes back with it in one round trip.
    let (mut back, wire) = returning_with(&rig.net, home(5002), ticket, &credential);
    let sent = rig.connect(&mut back, &wire);
    assert!(back.is_connected(), "{:?}", back.state());
    assert_eq!((back.peer_id(), sent), (Some(peer), 1));
    rig.steps(3, &mut [&mut back]);
    assert_eq!(rig.totals().resumed_one_rtt, 1);
    assert_eq!(rig.seen.resumed, vec![peer]);
}

/// The review's first assert for a goodbye's held slot (ADR-8733 (1), ADR-8858 A2): a replayed
/// flight spends no hold. The hold is spent, and the return counted, only when the returning
/// client confirms; the return is a new peer, in one round trip, under the id its accept named.
#[test]
fn a_replayed_resume_init_spends_no_hold_and_the_return_spends_it_at_confirmation() {
    let mut rig = Rig::new(|config| config.max_peers_per_ip = 1);
    let (mut ada, _, peer) = rig.joined(home(5000));
    let credential = ada.resumption().expect("credential");
    let ticket = ada.resume_ticket();
    ada.disconnect();
    rig.steps(5, &mut [&mut ada]);
    assert_eq!(rig.s.session().len(), 0, "the goodbye freed the live peer");
    assert_eq!(rig.seen.left, vec![peer]);

    // A flight whose reply is lost, then its bytes from the block and from outside.
    let (mut lost, wire) = returning_with(&rig.net, home(5001), ticket, &credential);
    wire.deafen(true);
    rig.now += STEP_MS;
    lost.flush(rig.now);
    rig.s.update(rig.now);
    let captured = wire.resume_init();
    drop(lost);
    assert_eq!(rig.s.pending_resumes(), 1);
    for round in 0..3 {
        let got = rig.play(home(6000 + round), &captured);
        assert_eq!(kind_of(&got[0]), Some(PacketKind::ResumeReply));
    }
    let got = rig.play(away(6000), &captured);
    assert_eq!(kind_of(&got[0]), Some(PacketKind::ResumeRetry));
    let t = rig.totals();
    assert_eq!(t.held_slot_returns, 0, "no hold was spent");
    assert_eq!((t.accepted, t.resumed_one_rtt), (1, 0));
    assert_eq!(rig.s.session().len(), 0, "no peer installed");
    assert_eq!(rig.seen.joined, vec![peer]);

    // The real return, from the hold's block: one round trip, and the hold goes at the
    // confirmation, not before.
    let (mut back, wire) = returning_with(&rig.net, home(5002), ticket, &credential);
    rig.now += STEP_MS;
    back.flush(rig.now);
    rig.s.receive(rig.now);
    back.receive(rig.now);
    assert!(back.is_connected(), "{:?}", back.state());
    assert_eq!(wire.sent(), 1);
    assert_eq!(rig.totals().held_slot_returns, 0, "answered, not yet confirmed");
    assert_eq!(rig.s.session().len(), 0);
    rig.now += STEP_MS;
    back.flush(rig.now);
    rig.s.receive(rig.now);
    rig.events();
    let new_peer = back.peer_id().expect("peer");
    assert_ne!(new_peer, peer, "a goodbye's return is a new peer");
    assert_eq!(rig.seen.joined, vec![peer, new_peer]);
    assert!(rig.seen.resumed.is_empty());
    let t = rig.totals();
    assert_eq!((t.held_slot_returns, t.resumed_one_rtt, t.resumes), (1, 1, 0));
    assert_eq!(rig.peer_addr(new_peer), Some(home(5002)));
    rig.steps(5, &mut [&mut back]);
    assert!(back.is_connected());

    // Single use: the hold is spent, so the same credential names nothing now. The flight is
    // refused to the full handshake, where the cap of 1 refuses the fresh join.
    let (mut again, wire) = returning_with(&rig.net, home(5003), ticket, &credential);
    rig.connect(&mut again, &wire);
    assert_eq!(rig.s.last_resume_refusal(), Some(ResumeRefusal::NoPeer));
    assert!(
        matches!(
            again.state(),
            ClientState::Ended(ClientEnd::Denied {
                reason: DenyReason::Full,
                ..
            })
        ),
        "{:?}",
        again.state()
    );
    assert_eq!(rig.totals().held_slot_returns, 1);
}

/// The review's second assert (ADR-8858 A3): flights are ordered by the client's own count. A
/// second flight with a higher number leaves the first one's pending row alone: the first still
/// gets its stored reply and still resumes the peer.
#[test]
fn a_newer_flight_does_not_disturb_the_pending_row_of_an_older_one() {
    let mut rig = Rig::new(|_| {});
    let (first, _, peer) = rig.joined(home(5000));
    let credential = first.resumption().expect("credential");
    let ticket = first.resume_ticket();

    let (mut one, wire_one) = returning_with(&rig.net, home(5001), ticket, &credential);
    wire_one.deafen(true);
    rig.now += STEP_MS;
    one.flush(rig.now);
    rig.s.update(rig.now);
    let (mut two, wire_two) = returning_with(&rig.net, home(5002), ticket, &credential);
    wire_two.deafen(true);
    rig.now += STEP_MS;
    two.flush(rig.now);
    rig.s.update(rig.now);
    assert_eq!(credential.flights(), 2);
    assert_eq!(rig.s.pending_resumes(), 2, "both wait");
    let handshakes = rig.totals().handshakes;

    // Flight 1's bytes still reach its own row.
    let got = rig.play(home(6000), &wire_one.resume_init());
    assert_eq!(kind_of(&got[0]), Some(PacketKind::ResumeReply));
    assert_eq!(rig.totals().handshakes, handshakes);

    // And flight 1's client, once it hears, is resumed as the peer.
    wire_one.deafen(false);
    let sent_before = wire_one.sent();
    for _ in 0..200 {
        rig.step(&mut [&mut one]);
        if one.is_connected() {
            break;
        }
    }
    assert!(one.is_connected(), "{:?}", one.state());
    assert_eq!(one.peer_id(), Some(peer));
    assert!(wire_one.sent() > sent_before, "it resent its flight");
    rig.steps(3, &mut [&mut one]);
    assert_eq!(rig.peer_addr(peer), Some(home(5001)));
    let t = rig.totals();
    assert_eq!((t.resumed_one_rtt, t.resumes), (1, 1));
    assert_eq!(t.handshakes, handshakes, "the resend was answered from the store");
    // The row left with the confirmation, and flight 2 with it: one use per ticket.
    assert_eq!(rig.s.pending_resumes(), 0);
    assert_eq!(t.resume_lapsed, 1);
    let got = rig.play(home(6001), &wire_two.resume_init());
    assert_eq!(
        kind_of(&got[0]),
        Some(PacketKind::Challenge),
        "refused to the full handshake"
    );
    assert_eq!(rig.s.last_resume_refusal(), Some(ResumeRefusal::Epoch));
    assert_eq!(rig.s.status().resume_refusals.get("resume_epoch"), Some(&1));
    drop(two);
}

/// A third pending resume is refused (ADR-8733 (3)(iv), ADR-8858 A3): a ticket never has more
/// than two. The third flight is the newest, so the one that goes is the lowest: its stored reply
/// is gone, its bytes played again get no answer and never replace a newer flight, and its
/// client, though it heard its reply, is not admitted.
#[test]
fn a_third_pending_resume_is_refused_the_lowest_flight_goes() {
    let mut rig = Rig::new(|_| {});
    let (first, _, peer) = rig.joined(home(5000));
    let credential = first.resumption().expect("credential");
    let ticket = first.resume_ticket();
    let old_addr = rig.peer_addr(peer).expect("live");

    // Flight 1 is answered and its client hears the reply, but has not sent anything yet.
    let (mut one, wire_one) = returning_with(&rig.net, home(5001), ticket, &credential);
    rig.now += STEP_MS;
    one.flush(rig.now);
    rig.s.receive(rig.now);
    one.receive(rig.now);
    assert!(one.is_connected(), "{:?}", one.state());
    // Flights 2 and 3, replies lost.
    let (mut two, wire_two) = returning_with(&rig.net, home(5002), ticket, &credential);
    wire_two.deafen(true);
    two.flush(rig.now);
    rig.s.receive(rig.now);
    assert_eq!(rig.s.pending_resumes(), 2);
    let (mut three, wire_three) = returning_with(&rig.net, home(5003), ticket, &credential);
    wire_three.deafen(true);
    three.flush(rig.now);
    rig.s.receive(rig.now);
    assert_eq!(credential.flights(), 3);
    assert_eq!(rig.s.pending_resumes(), 2, "never a third");
    assert_eq!(rig.totals().resume_lapsed, 1, "flight 1 gave way");
    let handshakes = rig.totals().handshakes;

    // Flight 1 played again: older than the highest, matching no row. Nothing comes back, no
    // curve work is done, and the two newer rows are untouched.
    assert!(rig.play(home(6000), &wire_one.resume_init()).is_empty());
    assert_eq!(rig.s.resume_refusals(ResumeRefusal::StaleFlight), 1);
    assert_eq!(
        rig.s.status().resume_refusals.get("resume_stale_flight"),
        Some(&1)
    );
    assert_eq!(rig.totals().handshakes, handshakes);
    assert_eq!(rig.s.pending_resumes(), 2);
    for wire in [&wire_two, &wire_three] {
        let got = rig.play(home(6001), &wire.resume_init());
        assert_eq!(
            kind_of(&got[0]),
            Some(PacketKind::ResumeReply),
            "its row still answers"
        );
    }

    // Flight 1's client confirms keys the server no longer holds: nobody is admitted.
    rig.now += STEP_MS;
    one.flush(rig.now);
    rig.s.receive(rig.now);
    rig.events();
    assert_eq!(
        rig.peer_addr(peer),
        Some(old_addr),
        "the refused flight resumed nobody"
    );
    assert_eq!(rig.totals().resumed_one_rtt, 0);
    assert!(rig.seen.resumed.is_empty());

    // Flight 3, the newest, is the one that resumes once its client hears.
    wire_three.deafen(false);
    for _ in 0..200 {
        rig.step(&mut [&mut three]);
        if three.is_connected() {
            break;
        }
    }
    assert_eq!(three.peer_id(), Some(peer), "{:?}", three.state());
    rig.steps(3, &mut [&mut three]);
    assert_eq!(rig.peer_addr(peer), Some(home(5003)));
    assert_eq!(rig.totals().resumed_one_rtt, 1);
    drop((one, two));
}

/// The review's third assert (ADR-8858 A4): a byte-identical replay is charged neither `ee` nor
/// `handshakes_per_ip_per_s` again. With a budget of two handshakes for the address, twenty
/// replays of flight 1 leave room for flight 2; flight 3 is the one over the budget.
#[test]
fn a_byte_identical_replay_is_not_charged_twice() {
    let mut rig = Rig::new(|config| config.handshakes_per_ip_per_s = 1);
    let (mut first, _, peer) = rig.joined(home(5000));
    let credential = first.resumption().expect("credential");
    let ticket = first.resume_ticket();
    // The join spent from the address's bucket; five seconds refill it to its burst of two.
    for _ in 0..500 {
        rig.step(&mut [&mut first]);
    }
    let before = rig.totals();

    let (mut one, wire_one) = returning_with(&rig.net, home(5001), ticket, &credential);
    wire_one.deafen(true);
    rig.now += STEP_MS;
    one.flush(rig.now);
    rig.s.receive(rig.now);
    assert_eq!(rig.totals().handshakes, before.handshakes + 1);
    let captured = wire_one.resume_init();
    drop(one);
    // Twenty copies inside the same 10 ms: each gets the stored reply.
    let mut socket = rig.net.bind(home(6000)).expect("bind");
    for _ in 0..20 {
        socket.send_to(&captured, server_addr()).expect("send");
    }
    rig.s.receive(rig.now);
    let mut replies = 0;
    let mut buf = [0u8; 2048];
    while let Some((n, _)) = socket.recv_from(&mut buf).expect("receive") {
        assert_eq!(kind_of(&buf[..n]), Some(PacketKind::ResumeReply));
        replies += 1;
    }
    assert_eq!(replies, 20);
    let t = rig.totals();
    assert_eq!(t.resume_replays, 20);
    assert_eq!(t.handshakes, before.handshakes + 1, "`ee` once");
    assert_eq!(t.handshakes_refused, before.handshakes_refused);

    // Flight 2 in the same instant: the second token is still there.
    let (mut two, wire_two) = returning_with(&rig.net, home(5002), ticket, &credential);
    wire_two.deafen(true);
    two.flush(rig.now);
    rig.s.receive(rig.now);
    let t = rig.totals();
    assert_eq!(
        t.handshakes,
        before.handshakes + 2,
        "the replays did not spend the address's handshake budget"
    );
    assert_eq!(rig.s.pending_resumes(), 2);
    assert_eq!(rig.s.last_resume_refusal(), None);

    // Flight 3 in the same instant is a third distinct flight: over the budget of two.
    let (mut three, wire_three) = returning_with(&rig.net, home(5003), ticket, &credential);
    wire_three.deafen(true);
    three.flush(rig.now);
    rig.s.receive(rig.now);
    let t = rig.totals();
    assert_eq!(
        t.handshakes,
        before.handshakes + 2,
        "no curve work over the budget"
    );
    assert_eq!(rig.s.last_resume_refusal(), Some(ResumeRefusal::Busy));
    assert_eq!(rig.s.resume_refusals(ResumeRefusal::Busy), 1);
    assert_eq!(rig.s.pending_resumes(), 2, "a refused flight replaces nothing");
    assert_eq!(rig.s.session().len(), 1);
    assert_eq!(rig.seen.joined, vec![peer]);
    drop((two, three));
}

/// A lost `ResumeReply`: the client's resend is the same flight and gets the stored reply; a new
/// attempt with the same credential is a second flight, inside the limit of two. Either way the
/// player is the same peer, and the peer is resumed once.
#[test]
fn a_lost_resume_reply_retried_is_the_same_peer() {
    // The same flight again.
    let mut rig = Rig::new(|_| {});
    let (first, _, peer) = rig.joined(home(5000));
    let (mut back, wire) = returning(&rig.net, home(5001), &first);
    wire.deafen(true);
    rig.steps(3, &mut [&mut back]);
    assert_eq!(rig.s.pending_resumes(), 1);
    assert!(!back.is_connected(), "the reply was lost");
    let handshakes = rig.totals().handshakes;
    wire.deafen(false);
    for _ in 0..300 {
        rig.step(&mut [&mut back]);
        if back.is_connected() {
            break;
        }
    }
    assert_eq!(back.peer_id(), Some(peer), "{:?}", back.state());
    rig.steps(3, &mut [&mut back]);
    let t = rig.totals();
    assert!(t.resume_replays >= 1, "{t:?}");
    assert_eq!(t.handshakes, handshakes, "the retry cost no key work");
    assert_eq!((t.resumed_one_rtt, t.resumes, t.resume_fallbacks), (1, 1, 0));
    assert_eq!(rig.seen.resumed, vec![peer]);
    assert_eq!(rig.s.session().len(), 1);

    // A new attempt: flight 2 of the same credential.
    let mut rig = Rig::new(|_| {});
    let (first, _, peer) = rig.joined(home(5000));
    let credential = first.resumption().expect("credential");
    let ticket = first.resume_ticket();
    let (mut lost, wire) = returning_with(&rig.net, home(5001), ticket, &credential);
    wire.deafen(true);
    rig.steps(3, &mut [&mut lost]);
    drop(lost);
    let (mut retry, wire) = returning_with(&rig.net, home(5002), ticket, &credential);
    let sent = rig.connect(&mut retry, &wire);
    assert_eq!(credential.flights(), 2);
    assert_eq!((retry.peer_id(), sent), (Some(peer), 1), "{:?}", retry.state());
    rig.steps(3, &mut [&mut retry]);
    let t = rig.totals();
    assert_eq!((t.resumed_one_rtt, t.resumes, t.resume_fallbacks), (1, 1, 0));
    assert_eq!(rig.seen.resumed, vec![peer], "resumed once");
    assert_eq!(rig.seen.joined, vec![peer], "never a second peer");
    assert_eq!(rig.s.session().len(), 1);
    assert_eq!(rig.s.pending_resumes(), 0, "the unheard flight went with the row");
}

/// A dormant player comes back in one round trip under its id; a ticket is single use per epoch:
/// once the client confirmed the new accept, the old credential is refused by name
/// (`resume_epoch`) from every address, and the resumed peer is untouched.
#[test]
fn a_dormant_peer_resumes_in_one_round_trip_and_its_ticket_is_single_use() {
    let mut rig = Rig::new(|_| {});
    let (first, _, peer) = rig.joined(home(5000));
    let credential = first.resumption().expect("credential");
    let ticket = first.resume_ticket();
    drop(first);
    rig.until_dormant(peer);
    assert_eq!(rig.seen.dormant, vec![peer]);

    let (mut back, wire) = returning_with(&rig.net, home(5001), ticket, &credential);
    let sent = rig.connect(&mut back, &wire);
    assert_eq!((back.peer_id(), sent), (Some(peer), 1), "{:?}", back.state());
    rig.steps(3, &mut [&mut back]);
    let t = rig.totals();
    assert_eq!(
        (t.resumes, t.resume_takeovers, t.resumed_one_rtt),
        (1, 0, 1),
        "from dormancy"
    );
    assert_eq!(rig.s.dormant_peers().count(), 0);
    assert_eq!(rig.seen.resumed, vec![peer]);

    // The spent credential, from its own block and from another.
    for (n, addr) in [home(5002), away(5002)].into_iter().enumerate() {
        let (mut stale, wire) = returning_with(&rig.net, addr, ticket, &credential);
        let sent = rig.connect(&mut stale, &wire);
        assert_eq!(
            rig.s.last_resume_refusal(),
            Some(ResumeRefusal::Epoch),
            "from {addr}"
        );
        assert_eq!(rig.s.resume_refusals(ResumeRefusal::Epoch), n as u64 + 1);
        // The full handshake it fell back to found its 16-byte ticket spent too: a new peer.
        assert!(stale.is_connected(), "{:?}", stale.state());
        assert_ne!(stale.peer_id(), Some(peer));
        assert_eq!(sent, 3 + n, "three round trips, plus the retry from outside");
    }
    rig.steps(3, &mut [&mut back]);
    assert!(back.is_connected(), "the resumed peer is untouched");
    assert_eq!(rig.peer_addr(peer), Some(home(5001)));
    assert_eq!(rig.totals().resumed_one_rtt, 1);
}

/// From outside the ticket's block the server asks for a cookie first: two round trips, one
/// fewer than the full handshake, and the peer moves to the new block when the client confirms.
#[test]
fn a_resume_from_another_block_takes_one_retry() {
    let mut rig = Rig::new(|_| {});
    let (first, _, peer) = rig.joined(home(5000));
    let (mut back, wire) = returning(&rig.net, away(5000), &first);
    let sent = rig.connect(&mut back, &wire);
    assert_eq!((back.peer_id(), sent), (Some(peer), 2), "{:?}", back.state());
    rig.steps(3, &mut [&mut back]);
    assert_eq!(rig.peer_addr(peer), Some(away(5000)));
    let t = rig.totals();
    assert_eq!((t.resumed_one_rtt, t.resume_fallbacks, t.resume_inits), (1, 0, 2));
    assert_eq!(rig.s.resume_refusals(ResumeRefusal::OtherBlock), 1);
    // The new ticket is for the new block: the next resume from there needs no retry.
    let (mut again, wire) = returning(&rig.net, away(5001), &back);
    let sent = rig.connect(&mut again, &wire);
    assert_eq!((again.peer_id(), sent), (Some(peer), 1), "{:?}", again.state());
}

/// An admission that fails at the confirmation is answered with a sealed `Disconnect` on the
/// pending link, and nothing else changes (ADR-8858 A1): here the address was banned after the
/// flight was answered. The dormant player it named is still held.
#[test]
fn an_admission_refused_at_confirmation_is_a_sealed_disconnect() {
    let mut rig = Rig::new(|_| {});
    let (first, _, peer) = rig.joined(home(5000));
    let credential = first.resumption().expect("credential");
    let ticket = first.resume_ticket();
    drop(first);
    rig.until_dormant(peer);

    let (mut back, wire) = returning_with(&rig.net, home(5001), ticket, &credential);
    rig.now += STEP_MS;
    back.flush(rig.now);
    rig.s.receive(rig.now);
    back.receive(rig.now);
    assert!(back.is_connected(), "{:?}", back.state());
    rig.s.ban_ip_exact(home(5001).ip(), "test", 600);
    let denied = rig.totals().denied;
    rig.now += STEP_MS;
    back.flush(rig.now);
    rig.s.receive(rig.now);
    rig.events();
    assert_eq!(rig.totals().denied, denied + 1);
    assert_eq!(rig.totals().resumed_one_rtt, 0);
    assert_eq!(rig.s.pending_resumes(), 0, "the flight is over");
    assert!(
        rig.s.dormant_peers().any(|(held, _)| held == peer),
        "the hold is untouched"
    );
    assert!(rig.seen.resumed.is_empty());
    back.receive(rig.now);
    assert!(
        matches!(
            back.state(),
            ClientState::Ended(ClientEnd::Disconnected(DisconnectReason::Banned))
        ),
        "{:?}",
        back.state()
    );
    assert_eq!(wire.sent(), 2, "the flight and the one record");
}

/// A flight whose first Noise message does not open under the ticket's PSK gets no answer at all
/// (ADR-8858 A6), and the status view names every refusal by its lowercase code beside the count
/// of one-round-trip resumes.
#[test]
fn the_status_names_the_resume_path_and_a_bad_psk_gets_no_answer() {
    let mut rig = Rig::new(|_| {});
    let (first, _, peer) = rig.joined(home(5000));
    let status = rig.s.status();
    assert!(status.resume_refusals.is_empty());
    assert_eq!(status.totals.resumed_one_rtt, 0);
    let text = ron::to_string(&status).expect("status");
    assert!(!text.contains("resume_refusals"), "absent while empty: {text}");
    assert!(text.contains("resumed_one_rtt:0"), "{text}");

    let (mut back, wire) = returning(&rig.net, home(5001), &first);
    let sent = rig.connect(&mut back, &wire);
    assert_eq!((back.peer_id(), sent), (Some(peer), 1));
    rig.steps(3, &mut [&mut back]);

    // The captured flight with one byte of its Noise message changed: not the PSK holder's.
    let mut init = decode_init(&wire.resume_init());
    let last = init.noise.len() - 1;
    init.noise[last] ^= 1;
    let forged = init.to_datagram().expect("encodes");
    let out_before = rig.totals().datagrams_out;
    assert!(rig.play(home(6000), &forged).is_empty(), "no answer");
    assert_eq!(rig.s.last_resume_refusal(), Some(ResumeRefusal::Psk));
    // The same flight unchanged: its ticket was spent by the confirmation.
    let got = rig.play(home(6000), &wire.resume_init());
    assert_eq!(kind_of(&got[0]), Some(PacketKind::Challenge));
    // And from outside the block.
    let got = rig.play(away(6000), &wire.resume_init());
    assert_eq!(kind_of(&got[0]), Some(PacketKind::ResumeRetry));
    assert_eq!(rig.totals().datagrams_out, out_before + 2);

    let status = rig.s.status();
    assert_eq!(status.totals.resumed_one_rtt, 1);
    assert_eq!(status.totals.resume_inits, 4);
    assert_eq!(
        status.totals.resume_fallbacks, 1,
        "only the challenge is a fallback"
    );
    let refusals = &status.resume_refusals;
    assert_eq!(refusals.len(), 3, "{refusals:?}");
    assert_eq!(refusals.get("resume_psk"), Some(&1));
    assert_eq!(refusals.get("resume_epoch"), Some(&1));
    assert_eq!(refusals.get("resume_other_block"), Some(&1));
    for code in refusals.keys() {
        assert!(
            ResumeRefusal::ALL.iter().any(|why| why.as_str() == code),
            "{code} is a documented code"
        );
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
    // The view round-trips with the map in it.
    let text = ron::to_string(&status).expect("status");
    assert!(text.contains("resume_refusals"), "{text}");
    let read: NetStatus = ron::from_str(&text).expect("reads");
    assert_eq!(read, status);
}
