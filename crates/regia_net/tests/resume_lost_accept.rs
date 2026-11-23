// SPDX-License-Identifier: MPL-2.0
//! A resume whose accept is lost (ADR-8535, spec 120 § 10.3).
//!
//! The server mints a new ticket on every accept and the presented one is spent. When the accept
//! of a resume is lost, the client still holds only the old ticket; before ADR-8535 its next
//! attempt presented a spent ticket and joined as a new peer (the `link_reconnect` wifi-to-cell
//! run 12). Now a resumed connection keeps the ticket it resumed with until its client proves it
//! holds the accept (its first authenticated packet): a retry with it is the same resume again,
//! and once the client confirmed, the ticket is spent for good, from any address.
//!
//! Transport level: `NetServer` and raw `NetClient`s on the in-memory network, manual time, plain
//! (loopback `Auto`) and sealed (`Required`) links.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use regia_net::client::{ClientConfig, NetClient};
use regia_net::server::{NetServer, ServerConfig, ServerEvent};
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};
use regia_net::{Channel, CryptoMode, NetGame, PeerId};

#[derive(Debug)]
struct Game;

impl NetGame for Game {
    type Message = u32;
    type Input = u32;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.resume_lost_accept";
    const PROTOCOL_VERSION: u32 = 1;
}

/// The resume ticket's size.
type Ticket = [u8; 16];

/// A client socket that can go deaf (everything inbound lost), as a phone's radio whose answer
/// never arrives.
struct DeafSocket {
    socket: LoopbackSocket,
    deaf: Arc<AtomicBool>,
    /// Datagrams this socket received, when the test wants the accept off the wire.
    capture: Option<Arc<Mutex<Vec<Vec<u8>>>>>,
    /// Datagrams this socket sent, when the test replays one later.
    sent: Option<Arc<Mutex<Vec<Vec<u8>>>>>,
}

impl DatagramSocket for DeafSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if let Some(sent) = &self.sent {
            sent.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(bytes.to_vec());
        }
        self.socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.socket.recv_from(buf)?;
            if let Some((n, _)) = got
                && let Some(capture) = &self.capture
            {
                capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(buf[..n].to_vec());
            }
            if got.is_none() || !self.deaf.load(Ordering::Relaxed) {
                return Ok(got);
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

type Client = NetClient<Game, DeafSocket>;

/// What the server reported, by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Joined(PeerId),
    Resumed(PeerId),
    Dormant(PeerId),
    Left(PeerId),
    Message(PeerId),
}

struct Rig {
    net: LoopbackNet,
    server: NetServer<Game, LoopbackSocket>,
    server_addr: SocketAddr,
    crypto: CryptoMode,
    now: u64,
    seen: Vec<Seen>,
}

impl Rig {
    fn new(port: u16, crypto: CryptoMode) -> Self {
        let net = LoopbackNet::new();
        let server_addr = SocketAddr::from(([127, 0, 0, 1], port));
        let server = NetServer::with_secret(
            net.bind(server_addr).expect("bind"),
            ServerConfig {
                resume_grace_ms: 60_000,
                crypto,
                ..ServerConfig::default()
            },
            [9; 32],
        );
        Self {
            net,
            server,
            server_addr,
            crypto,
            now: 0,
            seen: Vec::new(),
        }
    }

    /// A client on a fresh socket (a new address) that presents `resume`.
    fn client(&self, resume: Option<Ticket>) -> (Client, Arc<AtomicBool>) {
        let deaf = Arc::new(AtomicBool::new(false));
        let socket = DeafSocket {
            socket: self
                .net
                .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .expect("bind"),
            deaf: deaf.clone(),
            capture: None,
            sent: None,
        };
        let config = ClientConfig {
            resume,
            crypto: self.crypto,
            ..ClientConfig::default()
        };
        let client = NetClient::connect(socket, self.server_addr, config, &()).expect("connect");
        (client, deaf)
    }

    /// Like [`Self::client`], and keeps every datagram the client reads.
    fn client_tapped(&self, resume: Option<Ticket>) -> (Client, Arc<Mutex<Vec<Vec<u8>>>>) {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let socket = DeafSocket {
            socket: self
                .net
                .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .expect("bind"),
            deaf: Arc::new(AtomicBool::new(false)),
            capture: Some(capture.clone()),
            sent: None,
        };
        let config = ClientConfig {
            resume,
            crypto: self.crypto,
            ..ClientConfig::default()
        };
        let client = NetClient::connect(socket, self.server_addr, config, &()).expect("connect");
        (client, capture)
    }

    /// 10 ms: every client, then the server.
    fn step(&mut self, clients: &mut [&mut Client]) {
        self.now += 10;
        for c in clients.iter_mut() {
            c.update(self.now);
        }
        self.server.update(self.now);
        while let Some(e) = self.server.poll_event() {
            let seen = match e {
                ServerEvent::PeerJoined { peer, .. } => Seen::Joined(peer),
                ServerEvent::PeerResumed { peer, .. } => Seen::Resumed(peer),
                ServerEvent::PeerDormant { peer, .. } => Seen::Dormant(peer),
                ServerEvent::PeerLeft { peer, .. } => Seen::Left(peer),
                ServerEvent::Message { peer, .. } => Seen::Message(peer),
                _ => continue,
            };
            self.seen.push(seen);
        }
    }

    fn run(&mut self, ms: u64, clients: &mut [&mut Client]) {
        for _ in 0..ms / 10 {
            self.step(clients);
        }
    }

    /// A client that joins (or resumes with `resume`), hears the accept and confirms it with a
    /// game message the server receives. Returns it with the peer it came back as.
    fn heard(&mut self, resume: Option<Ticket>) -> (Client, PeerId) {
        let (mut c, _) = self.client(resume);
        for _ in 0..500 {
            if c.peer_id().is_some() {
                break;
            }
            self.step(&mut [&mut c]);
        }
        let peer = c.peer_id().expect("the client was accepted");
        let before = self.messages_from(peer);
        c.send(Channel::GAME_RPC, &7).expect("send");
        for _ in 0..200 {
            if self.messages_from(peer) > before {
                break;
            }
            self.step(&mut [&mut c]);
        }
        assert!(self.messages_from(peer) > before, "the server heard the client");
        (c, peer)
    }

    /// A client that resumes with `ticket` and never hears the accept (deaf from the step the
    /// server accepted it), then goes away (a reconnect attempt replaced). Returns the peer the
    /// server made of it.
    fn unheard(&mut self, ticket: Ticket) -> Seen {
        let (mut c, deaf) = self.client(Some(ticket));
        let from = self.seen.len();
        for _ in 0..500 {
            self.step(&mut [&mut c]);
            if let Some(s) = self.seen[from..]
                .iter()
                .find(|s| matches!(s, Seen::Joined(_) | Seen::Resumed(_)))
            {
                // The accept is in flight: it never arrives.
                deaf.store(true, Ordering::Relaxed);
                let s = *s;
                // A few handshake resends with the same nonce from the same address: answered
                // with the same accept again (lost again), never admitted as another join.
                let accepted = self.seen.len();
                self.run(600, &mut [&mut c]);
                assert!(c.peer_id().is_none(), "the client never heard the accept");
                assert_eq!(
                    self.count_from(accepted, |s| matches!(s, Seen::Joined(_) | Seen::Resumed(_))),
                    0,
                    "a resend is not a second join: {:?}",
                    self.seen
                );
                return s;
            }
        }
        panic!("the server never accepted the resume: {:?}", self.seen);
    }

    fn messages_from(&self, peer: PeerId) -> usize {
        self.seen.iter().filter(|s| **s == Seen::Message(peer)).count()
    }

    fn count(&self, f: impl Fn(&Seen) -> bool) -> usize {
        self.count_from(0, f)
    }

    fn count_from(&self, from: usize, f: impl Fn(&Seen) -> bool) -> usize {
        self.seen[from..].iter().filter(|s| f(s)).count()
    }

    /// A player that joined, confirmed, and went silent past the timeout: dormant, with its
    /// ticket.
    fn dormant_player(&mut self) -> (PeerId, Ticket) {
        let (c, peer) = self.heard(None);
        let ticket = c.resume_ticket().expect("a ticket");
        drop(c);
        self.run(35_000, &mut []);
        assert_eq!(self.count(|s| *s == Seen::Dormant(peer)), 1, "{:?}", self.seen);
        (peer, ticket)
    }
}

const MODES: [CryptoMode; 2] = [CryptoMode::Auto, CryptoMode::Required];

/// The wifi-to-cell run 12 case: a resume's accept is lost and the next attempt presents the same
/// ticket from a new address with a new nonce. It comes back as the same peer; the server sees one
/// join in all.
#[test]
fn a_lost_accept_does_not_turn_the_retry_into_a_new_join() {
    for (i, crypto) in MODES.into_iter().enumerate() {
        let mut rig = Rig::new(7300 + i as u16, crypto);
        let (peer, ticket) = rig.dormant_player();
        assert_eq!(rig.unheard(ticket), Seen::Resumed(peer), "{crypto:?}");
        let (c, back) = rig.heard(Some(ticket));
        assert_eq!(back, peer, "{crypto:?}: the retry is the same peer");
        assert_eq!(
            rig.count(|s| matches!(s, Seen::Joined(_))),
            1,
            "{crypto:?}: {:?}",
            rig.seen
        );
        assert_eq!(rig.count(|s| matches!(s, Seen::Left(_))), 0, "{crypto:?}");
        assert_eq!(rig.server.session().len(), 1, "{crypto:?}");
        let t = rig.server.totals();
        assert_eq!(
            (t.resumes, t.resume_retries, t.resumes_refused),
            (2, 1, 0),
            "{crypto:?}: {t:?}"
        );
        assert_ne!(
            c.resume_ticket(),
            Some(ticket),
            "a fresh ticket on the heard accept"
        );
    }
}

/// Two lost accepts in a row: the third attempt, with the same ticket still, is the same peer.
#[test]
fn two_lost_accepts_in_a_row_still_come_back_as_the_same_peer() {
    for (i, crypto) in MODES.into_iter().enumerate() {
        let mut rig = Rig::new(7310 + i as u16, crypto);
        let (peer, ticket) = rig.dormant_player();
        assert_eq!(rig.unheard(ticket), Seen::Resumed(peer));
        assert_eq!(rig.unheard(ticket), Seen::Resumed(peer));
        let (_c, back) = rig.heard(Some(ticket));
        assert_eq!(back, peer, "{crypto:?}");
        assert_eq!(rig.count(|s| matches!(s, Seen::Joined(_))), 1, "{crypto:?}");
        let t = rig.server.totals();
        assert_eq!(
            (t.resumes, t.resume_retries, t.resumes_refused),
            (3, 2, 0),
            "{crypto:?}: {t:?}"
        );
    }
}

/// The unheard resume's connection times out before the client's next attempt (the slow case of
/// a long backoff): the peer is dormant again and the old ticket still brings it back.
#[test]
fn the_retry_ticket_survives_the_unheard_connection_timing_out() {
    for (i, crypto) in MODES.into_iter().enumerate() {
        let mut rig = Rig::new(7320 + i as u16, crypto);
        let (peer, ticket) = rig.dormant_player();
        assert_eq!(rig.unheard(ticket), Seen::Resumed(peer));
        rig.run(35_000, &mut []);
        assert_eq!(
            rig.count(|s| *s == Seen::Dormant(peer)),
            2,
            "{crypto:?}: {:?}",
            rig.seen
        );
        let (_c, back) = rig.heard(Some(ticket));
        assert_eq!(back, peer, "{crypto:?}");
        assert_eq!(rig.count(|s| matches!(s, Seen::Joined(_))), 1, "{crypto:?}");
        assert_eq!(rig.server.totals().resume_retries, 1, "{crypto:?}");
    }
}

/// Adversarial: once the resumed client confirmed its accept, the ticket it resumed with is spent.
/// A third party replaying it from another address (another cookie, another nonce) joins as a new
/// peer and the resumed client keeps its connection.
#[test]
fn a_ticket_is_spent_once_the_client_confirms_the_resume() {
    for (i, crypto) in MODES.into_iter().enumerate() {
        let mut rig = Rig::new(7330 + i as u16, crypto);
        let (peer, ticket) = rig.dormant_player();
        let (mut owner, back) = rig.heard(Some(ticket));
        assert_eq!(back, peer);
        let (mut thief, _) = rig.client(Some(ticket));
        for _ in 0..500 {
            if thief.peer_id().is_some() {
                break;
            }
            rig.step(&mut [&mut owner, &mut thief]);
        }
        let stolen = thief.peer_id().expect("the replay is a fresh join");
        assert_ne!(stolen, peer, "{crypto:?}: a spent ticket names no peer");
        rig.run(1_000, &mut [&mut owner, &mut thief]);
        assert_eq!(
            owner.peer_id(),
            Some(peer),
            "{crypto:?}: the owner keeps its connection"
        );
        let t = rig.server.totals();
        assert_eq!(
            (t.resumes, t.resume_retries, t.resumes_refused),
            (1, 0, 1),
            "{crypto:?}: {t:?}"
        );
        assert_eq!(rig.count(|s| *s == Seen::Resumed(peer)), 1, "{crypto:?}");
    }
}

/// Adversarial: a confirmed retry spends the ticket too, and the ticket stays spent when the
/// resumed peer later goes dormant; the newest ticket is the only one that brings it back.
#[test]
fn a_spent_ticket_stays_spent_through_a_later_dormancy() {
    for (i, crypto) in MODES.into_iter().enumerate() {
        let mut rig = Rig::new(7340 + i as u16, crypto);
        let (peer, old) = rig.dormant_player();
        assert_eq!(rig.unheard(old), Seen::Resumed(peer));
        let (owner, back) = rig.heard(Some(old));
        assert_eq!(back, peer);
        let newest = owner.resume_ticket().expect("a ticket");
        drop(owner);
        rig.run(35_000, &mut []);
        assert_eq!(
            rig.count(|s| *s == Seen::Dormant(peer)),
            2,
            "{crypto:?}: {:?}",
            rig.seen
        );
        // The replay from elsewhere.
        let (thief, stolen) = rig.heard(Some(old));
        assert_ne!(stolen, peer, "{crypto:?}: spent through the dormancy");
        drop(thief);
        let (_c, back) = rig.heard(Some(newest));
        assert_eq!(back, peer, "{crypto:?}: the newest ticket still resumes");
        let t = rig.server.totals();
        assert_eq!((t.resume_retries, t.resumes_refused), (1, 1), "{crypto:?}: {t:?}");
    }
}

/// The wifi-to-cell run 12 case itself (ADR-8535): the resume's accept is lost and the same
/// attempt resends its handshake from the same address with the same nonce. The server finds the
/// resumed peer by its address and answers with the same accept; the client comes back as the same
/// peer, with no retry and no second join.
#[test]
fn a_resend_after_a_lost_resume_accept_gets_the_same_accept() {
    for (i, crypto) in MODES.into_iter().enumerate() {
        let mut rig = Rig::new(7350 + i as u16, crypto);
        let (peer, ticket) = rig.dormant_player();
        let (mut c, deaf) = rig.client(Some(ticket));
        let from = rig.seen.len();
        for _ in 0..500 {
            rig.step(&mut [&mut c]);
            if rig.count_from(from, |s| matches!(s, Seen::Resumed(_) | Seen::Joined(_))) > 0 {
                break;
            }
        }
        assert_eq!(rig.seen.last(), Some(&Seen::Resumed(peer)), "{crypto:?}");
        // The first accept and a few resent ones are lost; then the path heals.
        deaf.store(true, Ordering::Relaxed);
        rig.run(600, &mut [&mut c]);
        deaf.store(false, Ordering::Relaxed);
        for _ in 0..200 {
            if c.peer_id().is_some() {
                break;
            }
            rig.step(&mut [&mut c]);
        }
        assert_eq!(c.peer_id(), Some(peer), "{crypto:?}: the same peer");
        assert_eq!(
            rig.count(|s| matches!(s, Seen::Joined(_))),
            1,
            "{crypto:?}: {:?}",
            rig.seen
        );
        assert_eq!(
            rig.count(|s| matches!(s, Seen::Resumed(_))),
            1,
            "{crypto:?}: one resume"
        );
        let t = rig.server.totals();
        assert_eq!(
            (t.resumes, t.resume_retries, t.resumes_refused),
            (1, 0, 0),
            "{crypto:?}: {t:?}"
        );
        assert_eq!(rig.server.session().len(), 1, "{crypto:?}");
    }
}

/// The session token carried by the first `Accept` this client read.
fn first_accept_token(captured: &Mutex<Vec<Vec<u8>>>) -> u32 {
    let datagrams = captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    datagrams
        .iter()
        .find_map(|datagram| {
            let (kind, body) = regia_net::handshake::split_datagram(datagram)?;
            if kind != regia_net::handshake::PacketKind::Accept {
                return None;
            }
            regia_net::handshake::Accept::decode(body)
                .ok()
                .map(|accept| accept.session_token)
        })
        .expect("the client read an Accept")
}

/// Presenting the newer ticket before the first resume is confirmed replaces the one-deep retry
/// and drops the ticket the previous attempt still holds. A join that still carries that older
/// ticket must come back as the same peer: on wifi-to-cell the client can hear accept T1, refresh,
/// and still have a datagram from the previous attempt in flight with T0.
#[test]
fn an_unconfirmed_older_ticket_must_not_open_a_second_slot() {
    for (i, crypto) in MODES.into_iter().enumerate() {
        let mut rig = Rig::new(7360 + i as u16, crypto);
        let (peer, t0) = rig.dormant_player();
        let (mut a, _) = rig.client(Some(t0));
        let from = rig.seen.len();
        for _ in 0..500 {
            rig.step(&mut [&mut a]);
            if rig.count_from(from, |s| matches!(s, Seen::Resumed(_))) > 0 {
                break;
            }
        }
        assert_eq!(rig.seen.last(), Some(&Seen::Resumed(peer)), "{crypto:?}");
        // The accept is already in A's socket. Read it and do not flush: a flush would send the
        // payload that spends the retry, and the hole is the window before that confirmation.
        a.receive(rig.now);
        let t1 = a.resume_ticket().expect("the client heard the minted ticket");
        assert_ne!(t1, t0, "{crypto:?}");
        assert_eq!(a.peer_id(), Some(peer), "{crypto:?}");
        // B presents T1. The server keeps only that ticket as the retry and drops T0.
        // B reads its accept without a flush as well (ADR-8623 lane): `update` would flush, and
        // the first packet B sends on its connection is the confirmation that spends T0 for good
        // (`a_ticket_is_spent_once_the_client_confirms_the_resume`; the confirmed case is
        // `a_confirmed_takeover_spends_the_older_ticket`). The hole is the unconfirmed window.
        let (mut b, _) = rig.client(Some(t1));
        let from = rig.seen.len();
        for _ in 0..500 {
            rig.step(&mut [&mut b]);
            if rig.count_from(from, |s| matches!(s, Seen::Resumed(_))) > 0 {
                break;
            }
        }
        b.receive(rig.now);
        assert_eq!(b.peer_id(), Some(peer), "{crypto:?}: T1 takes over the same peer");
        // C presents T0, the ticket the previous attempt still holds.
        let (mut c, _) = rig.client(Some(t0));
        for _ in 0..500 {
            if c.peer_id().is_some() {
                break;
            }
            rig.step(&mut [&mut c]);
        }
        assert_eq!(
            c.peer_id(),
            Some(peer),
            "{crypto:?}: an unconfirmed older ticket must not open a second slot (peers {}, seen {:?})",
            rig.server.session().len(),
            rig.seen
        );
        assert_eq!(rig.server.session().len(), 1, "{crypto:?}");
        assert_eq!(
            rig.count(|s| matches!(s, Seen::Joined(_))),
            1,
            "{crypto:?}: {:?}",
            rig.seen
        );
    }
}

/// A live takeover mints a new plain session token and must forget the previous one. A payload
/// from a new address that still carries the token from the accept before the takeover is not
/// this peer: it must not be counted as a rebind and must not start a path probe.
#[test]
fn a_revoked_plain_session_token_must_not_select_the_peer() {
    let mut rig = Rig::new(7370, CryptoMode::Off);
    let (mut owner, capture) = rig.client_tapped(None);
    for _ in 0..500 {
        if owner.peer_id().is_some() {
            break;
        }
        rig.step(&mut [&mut owner]);
    }
    let peer = owner.peer_id().expect("the owner was accepted");
    let ticket = owner.resume_ticket().expect("a ticket");
    let old_token = first_accept_token(&capture);
    let before_msg = rig.messages_from(peer);
    owner.send(Channel::GAME_RPC, &7).expect("send");
    for _ in 0..200 {
        if rig.messages_from(peer) > before_msg {
            break;
        }
        rig.step(&mut [&mut owner]);
    }
    assert!(rig.messages_from(peer) > before_msg, "the server heard the owner");
    let (mut taker, _) = rig.client(Some(ticket));
    for _ in 0..500 {
        if taker.peer_id() == Some(peer) {
            break;
        }
        rig.step(&mut [&mut taker]);
    }
    assert_eq!(
        taker.peer_id(),
        Some(peer),
        "the newest ticket takes over the live peer"
    );
    assert!(
        rig.server.totals().resume_takeovers >= 1,
        "the second resume replaced a live connection"
    );
    let mut attacker = rig.net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let datagram =
        regia_net::handshake::connected_header(regia_net::handshake::PacketKind::Payload, old_token);
    attacker.send_to(&datagram, rig.server_addr).expect("send");
    let before = rig.server.totals();
    rig.now += 10;
    rig.server.update(rig.now);
    let after = rig.server.totals();
    assert_eq!(
        after.rebinds_refused, before.rebinds_refused,
        "a session token from before a live takeover must not select the peer ({before:?} -> {after:?})"
    );
    assert_eq!(
        after.malformed,
        before.malformed + 1,
        "an unknown token is not a rebind: {after:?}"
    );
    assert_eq!(
        after.path_probes, before.path_probes,
        "a revoked token must not start a path move: {after:?}"
    );
}

impl Rig {
    /// A client that resumes with `ticket`, reads the accept and does not flush, so it never
    /// confirms. Returns it with the ticket the accept carried.
    fn resumed_unconfirmed(&mut self, ticket: Ticket, peer: PeerId) -> (Client, Ticket) {
        let (mut c, _) = self.client(Some(ticket));
        let from = self.seen.len();
        for _ in 0..500 {
            self.step(&mut [&mut c]);
            if self.count_from(from, |s| matches!(s, Seen::Resumed(_) | Seen::Joined(_))) > 0 {
                break;
            }
        }
        assert_eq!(self.seen.last(), Some(&Seen::Resumed(peer)), "{:?}", self.seen);
        c.receive(self.now);
        let newer = c.resume_ticket().expect("the client heard the minted ticket");
        assert_ne!(newer, ticket);
        (c, newer)
    }

    /// A fresh client presenting `ticket` until the server answers; the peer it came back as.
    fn present(&mut self, ticket: Ticket) -> PeerId {
        let (mut c, _) = self.client(Some(ticket));
        for _ in 0..500 {
            if let Some(peer) = c.peer_id() {
                return peer;
            }
            self.step(&mut [&mut c]);
        }
        panic!("no answer to {ticket:?}: {:?}", self.seen);
    }
}

/// ADR-8623: three resumes in a row, none confirmed (each attempt heard its accept and was then
/// replaced by the next one presenting the ticket it heard). Every ticket of that chain still names
/// the peer. Once a client confirms, the whole chain is spent: each old ticket is a fresh join and
/// the owner keeps the slot.
#[test]
fn an_unconfirmed_chain_of_three_keeps_every_ticket_until_a_confirmation() {
    for (i, crypto) in MODES.into_iter().enumerate() {
        let mut rig = Rig::new(7380 + i as u16, crypto);
        let (peer, t0) = rig.dormant_player();
        let (_a, t1) = rig.resumed_unconfirmed(t0, peer);
        let (_b, t2) = rig.resumed_unconfirmed(t1, peer);
        let (_c, t3) = rig.resumed_unconfirmed(t2, peer);
        for (n, old) in [t0, t1, t2].into_iter().enumerate() {
            let (_d, _) = rig.resumed_unconfirmed(old, peer);
            assert_eq!(rig.server.session().len(), 1, "{crypto:?}: ticket {n}");
        }
        assert_eq!(
            rig.count(|s| matches!(s, Seen::Joined(_))),
            1,
            "{crypto:?}: {:?}",
            rig.seen
        );
        // The client confirms with the newest ticket it heard (t3 is superseded by now too).
        let (mut owner, back) = rig.heard(Some(t3));
        assert_eq!(back, peer, "{crypto:?}");
        let resumes_refused = rig.server.totals().resumes_refused;
        for (n, old) in [t0, t1, t2, t3].into_iter().enumerate() {
            let fresh = rig.present(old);
            assert_ne!(
                fresh, peer,
                "{crypto:?}: ticket {n} is spent after the confirmation"
            );
            rig.run(50, &mut [&mut owner]);
        }
        assert_eq!(
            rig.server.totals().resumes_refused,
            resumes_refused + 4,
            "{crypto:?}"
        );
        assert_eq!(
            owner.peer_id(),
            Some(peer),
            "{crypto:?}: the owner keeps its slot"
        );
    }
}

/// The confirmed side of `an_unconfirmed_older_ticket_must_not_open_a_second_slot`: B takes over
/// with T1 and `update`s (read and flush), so its first packet confirms the accept. T0 is spent
/// for good: a join presenting it is a fresh join (the spec's answer to a spent ticket, as in
/// `a_ticket_is_spent_once_the_client_confirms_the_resume`) and never takes B's live connection.
#[test]
fn a_confirmed_takeover_spends_the_older_ticket() {
    for (i, crypto) in MODES.into_iter().enumerate() {
        let mut rig = Rig::new(7390 + i as u16, crypto);
        let (peer, t0) = rig.dormant_player();
        let (_a, t1) = rig.resumed_unconfirmed(t0, peer);
        let (mut b, _) = rig.client(Some(t1));
        for _ in 0..500 {
            if b.peer_id() == Some(peer) {
                break;
            }
            rig.step(&mut [&mut b]);
        }
        assert_eq!(b.peer_id(), Some(peer), "{crypto:?}");
        rig.run(100, &mut [&mut b]);
        let refused = rig.server.totals().resumes_refused;
        let (mut c, _) = rig.client(Some(t0));
        for _ in 0..500 {
            if c.peer_id().is_some() {
                break;
            }
            rig.step(&mut [&mut b, &mut c]);
        }
        assert_ne!(
            c.peer_id(),
            Some(peer),
            "{crypto:?}: T0 was spent by B's confirmation"
        );
        assert_eq!(rig.server.totals().resumes_refused, refused + 1, "{crypto:?}");
        rig.run(1_000, &mut [&mut b, &mut c]);
        assert_eq!(b.peer_id(), Some(peer), "{crypto:?}: B keeps its connection");
    }
}

/// ADR-8623: a stale copy of a join answer. Plain link: A resumes with T0 and confirms; B (the same
/// player's next attempt) takes over with A's ticket and confirms, so T0 and T1 are spent. A copy
/// of A's join answer still in flight then arrives from A's old address with A's nonce and a
/// cookie that still verifies. It is not a new client and must not make a second peer.
#[test]
fn a_stale_join_answer_after_a_takeover_makes_no_second_peer() {
    let mut rig = Rig::new(7400, CryptoMode::Off);
    let (peer, t0) = rig.dormant_player();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let socket = DeafSocket {
        socket: rig.net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
        deaf: Arc::new(AtomicBool::new(false)),
        capture: None,
        sent: Some(sent.clone()),
    };
    let a_addr = socket.local_addr().expect("addr");
    let config = ClientConfig {
        resume: Some(t0),
        crypto: CryptoMode::Off,
        ..ClientConfig::default()
    };
    let mut a: Client = NetClient::connect(socket, rig.server_addr, config, &()).expect("connect");
    for _ in 0..500 {
        if a.peer_id() == Some(peer) {
            break;
        }
        rig.step(&mut [&mut a]);
    }
    assert_eq!(a.peer_id(), Some(peer), "A resumed");
    rig.run(100, &mut [&mut a]);
    let t1 = a.resume_ticket().expect("a ticket");
    let (mut b, _) = rig.client(Some(t1));
    for _ in 0..500 {
        if b.peer_id() == Some(peer) {
            break;
        }
        rig.step(&mut [&mut b]);
    }
    assert_eq!(b.peer_id(), Some(peer), "B took over");
    rig.run(100, &mut [&mut b]);
    // A's attempt is gone; a copy of its join answer arrives from its old address.
    drop(a);
    let answers: Vec<Vec<u8>> = sent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|d| {
            regia_net::handshake::split_datagram(d)
                .is_some_and(|(kind, _)| kind == regia_net::handshake::PacketKind::ChallengeResponse)
        })
        .cloned()
        .collect();
    assert!(!answers.is_empty(), "A sent its join answer");
    let mut replay = rig.net.bind(a_addr).expect("rebind A's address");
    let refused = rig.server.totals().handshakes_refused;
    for answer in &answers {
        replay.send_to(answer, rig.server_addr).expect("send");
    }
    rig.run(100, &mut [&mut b]);
    assert_eq!(
        rig.count(|s| matches!(s, Seen::Joined(_))),
        1,
        "a stale answer is not a new join: {:?}",
        rig.seen
    );
    assert_eq!(rig.server.session().len(), 1);
    assert!(rig.server.totals().handshakes_refused > refused);
    assert_eq!(b.peer_id(), Some(peer), "B keeps its connection");
}
