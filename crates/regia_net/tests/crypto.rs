// SPDX-License-Identifier: MPL-2.0
//! Transport encryption end to end (ADR-1901), over the in-memory network with an on-path
//! [`Middlebox`] between client and server. The box sees every datagram and can tamper with it,
//! drop it, replay it or inject its own, which is everything a network attacker can do short of
//! breaking the cipher. Addresses are non-loopback, so `CryptoMode::Auto` seals. Time is a step
//! counter; nothing sleeps.

mod common;

use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::rc::Rc;

use common::{Duel, DuelMsg, Move, Team};
use regia_net::crypto::{
    CRYPTO_VERSION, ClientHandshake, CryptoMode, KEY_INIT_BYTES, KeyInit, KeyReply, PLAIN_VERSION,
    SEALED_RESERVED_HEADER_BYTES, SealedKind, ServerIdentity, ServerKey, accept_handshake, prologue,
};
use regia_net::handshake::{
    Challenge, ChallengeResponse, ConnectRequest, Deny, PacketKind, connected_header, split_datagram,
};
use regia_net::prelude::*;
use regia_net::session::NetSession;
use regia_net::session::lifecycle::{ClientSessionEvent, HostOptions, SessionSettings};
use regia_net::session::manifest::ContentManifest;
use regia_net::session::net_session::{ConnectOptions, SessionError};
use regia_net::session::proto::SessionMsg;
use regia_net::{LoopbackSocket, NetCodec, protocol_id};

const STEP_MS: u64 = 10;
const SERVER_ADDR: ([u8; 4], u16) = ([10, 0, 0, 1], 7777);
const CLIENT_ADDR: ([u8; 4], u16) = ([10, 0, 0, 2], 5000);
const BOX_ADDR: ([u8; 4], u16) = ([10, 0, 0, 9], 7000);
/// A marker no ciphertext contains by chance, used to prove what the wire can and cannot show.
const MARKER: &[u8] = b"attack at dawn: the secret plan";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dir {
    ToServer,
    ToClient,
}

type Rule = Box<dyn FnMut(Dir, &[u8]) -> Vec<Vec<u8>>>;

/// The on-path attacker. Every datagram passes `rule`, which returns what to forward.
struct Middlebox {
    socket: LoopbackSocket,
    server: SocketAddr,
    client: SocketAddr,
    /// Also gets every datagram for the client: an attacker who reads the victim's traffic.
    reader: Option<SocketAddr>,
    rule: Rule,
    log: Vec<(Dir, Vec<u8>)>,
}

impl Middlebox {
    fn pump(&mut self) {
        let mut buf = [0u8; 2048];
        while let Some((n, from)) = self.socket.recv_from(&mut buf).expect("io") {
            let dir = if from == self.server {
                Dir::ToClient
            } else {
                Dir::ToServer
            };
            self.log.push((dir, buf[..n].to_vec()));
            for out in (self.rule)(dir, &buf[..n]) {
                self.inject(dir, &out);
                if let (Dir::ToClient, Some(reader)) = (dir, self.reader) {
                    self.socket.send_to(&out, reader).expect("send");
                }
            }
        }
    }

    fn inject(&mut self, dir: Dir, bytes: &[u8]) {
        let to = if dir == Dir::ToServer {
            self.server
        } else {
            self.client
        };
        self.socket.send_to(bytes, to).expect("send");
    }

    fn wire_contains(&self, needle: &[u8]) -> bool {
        self.log
            .iter()
            .any(|(_, d)| d.windows(needle.len()).any(|w| w == needle))
    }

    fn sent(&self, dir: Dir, kind: PacketKind) -> Vec<Vec<u8>> {
        self.log
            .iter()
            .filter(|(d, bytes)| *d == dir && split_datagram(bytes).is_some_and(|(k, _)| k == kind))
            .map(|(_, bytes)| bytes.clone())
            .collect()
    }
}

struct Rig {
    net: LoopbackNet,
    server: NetServer<Duel, LoopbackSocket>,
    client: NetClient<Duel, LoopbackSocket>,
    mid: Middlebox,
    now: u64,
}

fn forward() -> Rule {
    Box::new(|_, bytes| vec![bytes.to_vec()])
}

fn rig(
    edit_server: impl FnOnce(&mut ServerConfig),
    edit_client: impl FnOnce(&mut ClientConfig),
    rule: Rule,
) -> Rig {
    let net = LoopbackNet::new();
    let (server_addr, box_addr) = (SocketAddr::from(SERVER_ADDR), SocketAddr::from(BOX_ADDR));
    let client_addr = SocketAddr::from(CLIENT_ADDR);
    let mut server_config = ServerConfig::default();
    edit_server(&mut server_config);
    let server = NetServer::with_secret(net.bind(server_addr).expect("bind"), server_config, [7; 32]);
    let mut client_config = ClientConfig {
        name: "ada".into(),
        ..ClientConfig::default()
    };
    edit_client(&mut client_config);
    let client = NetClient::connect(
        net.bind(client_addr).expect("bind"),
        box_addr,
        client_config,
        &Team(1),
    )
    .expect("join fits");
    let mid = Middlebox {
        socket: net.bind(box_addr).expect("bind"),
        server: server_addr,
        client: client_addr,
        reader: None,
        rule,
        log: Vec::new(),
    };
    Rig {
        net,
        server,
        client,
        mid,
        now: 0,
    }
}

impl Rig {
    fn step(&mut self) {
        self.now += STEP_MS;
        self.client.update(self.now);
        self.mid.pump();
        self.server.update(self.now);
        self.mid.pump();
    }

    fn steps(&mut self, n: usize) {
        for _ in 0..n {
            self.step();
        }
    }

    fn connect(&mut self) {
        for _ in 0..200 {
            if self.client.is_connected() {
                return;
            }
            self.step();
        }
        panic!("no connection: {:?}", self.client.state());
    }

    fn ended(&self) -> Option<&ClientEnd> {
        match self.client.state() {
            ClientState::Ended(end) => Some(end),
            _ => None,
        }
    }

    /// Steps until the client ended, which the attack under test must cause.
    fn run_to_end(&mut self) -> ClientEnd {
        for _ in 0..1000 {
            if let Some(end) = self.ended() {
                return end.clone();
            }
            self.step();
        }
        panic!("the client never ended: {:?}", self.client.state());
    }

    fn server_chats(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(e) = self.server.poll_event() {
            if let ServerEvent::Message {
                message: DuelMsg::Chat(text),
                ..
            } = e
            {
                out.push(text);
            }
        }
        out
    }

    fn client_chats(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(e) = self.client.poll_event() {
            if let ClientEvent::Message {
                message: DuelMsg::Chat(text),
                ..
            } = e
            {
                out.push(text);
            }
        }
        out
    }

    fn chat(&mut self, text: &str) {
        self.client
            .send(Channel::GAME_RPC, &DuelMsg::Chat(text.into()))
            .expect("queued");
    }

    fn peer(&self) -> PeerId {
        self.server.session().peers().next().expect("a peer").id
    }

    /// A second client that reads the victim's replies and sends through the victim's address, so
    /// the server sees one source: everything an on-path attacker without the session keys can do.
    fn spoofer(&mut self, password: Option<&str>) -> NetClient<Duel, LoopbackSocket> {
        let addr = SocketAddr::from(([10, 0, 0, 66], 6666));
        self.mid.reader = Some(addr);
        let config = ClientConfig {
            name: "mallory".into(),
            password: password.map(str::to_owned),
            ..ClientConfig::default()
        };
        NetClient::connect(
            self.net.bind(addr).expect("bind"),
            SocketAddr::from(BOX_ADDR),
            config,
            &Team(1),
        )
        .expect("join fits")
    }
}

fn text() -> String {
    String::from_utf8(MARKER.to_vec()).expect("ascii")
}

#[test]
fn auto_seals_non_loopback_peers() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.connect();
    assert!(r.client.is_sealed());
    assert!(r.server.peer_sealed(r.peer()));
    assert_eq!(r.server.totals().handshakes, 1);
    assert_eq!(r.client.totals().handshakes, 1);
    assert_eq!(
        r.client.server_key(),
        r.server.server_key(),
        "the client learned the key"
    );
    let status = r.server.status();
    assert_eq!(status.crypto.mode, "auto");
    assert_eq!(status.crypto.sealed_peers, 1);
    assert!(status.crypto.sealed);
    assert!(r.server.peer_rows()[0].sealed);
    assert!(r.client.status().crypto.sealed);
}

#[test]
fn auto_leaves_loopback_plain_and_required_seals_it() {
    fn on_loopback(server_mode: CryptoMode, client_mode: CryptoMode) -> bool {
        let net = LoopbackNet::new();
        let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
        let mut server: NetServer<Duel, _> = NetServer::with_secret(
            net.bind(server_addr).expect("bind"),
            ServerConfig {
                crypto: server_mode,
                ..ServerConfig::default()
            },
            [1; 32],
        );
        let mut client: NetClient<Duel, _> = NetClient::connect(
            net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
            server_addr,
            ClientConfig {
                crypto: client_mode,
                ..ClientConfig::default()
            },
            &Team(0),
        )
        .expect("fits");
        for step in 0..100 {
            client.update(step * STEP_MS);
            server.update(step * STEP_MS);
        }
        assert!(client.is_connected(), "{server_mode:?} {client_mode:?}");
        client.is_sealed()
    }
    assert!(!on_loopback(CryptoMode::Auto, CryptoMode::Auto));
    assert!(!on_loopback(CryptoMode::Off, CryptoMode::Off));
    assert!(on_loopback(CryptoMode::Required, CryptoMode::Auto));
    assert!(on_loopback(CryptoMode::Auto, CryptoMode::Required));
    assert!(on_loopback(CryptoMode::Required, CryptoMode::Required));
}

#[test]
fn nothing_readable_crosses_a_sealed_link_and_the_control_shows_the_test_can_see() {
    let run = |mode: CryptoMode| {
        let mut r = rig(|s| s.crypto = mode, |c| c.crypto = mode, forward());
        r.connect();
        r.chat(&text());
        r.steps(20);
        assert_eq!(r.server_chats(), vec![text()]);
        r.server
            .send(r.peer(), Channel::GAME_RPC, &DuelMsg::Chat(text()))
            .expect("queued");
        r.steps(20);
        assert_eq!(r.client_chats(), vec![text()]);
        (r.mid.wire_contains(MARKER), r.mid.wire_contains(b"ada"))
    };
    assert_eq!(
        run(CryptoMode::Off),
        (true, true),
        "control: plain traffic is visible to the box"
    );
    assert_eq!(
        run(CryptoMode::Auto),
        (false, false),
        "sealed: neither message nor join name"
    );
}

#[test]
fn every_tampered_byte_and_truncation_is_dropped_and_the_session_survives() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.connect();
    r.chat(&text());
    r.steps(20);
    assert_eq!(r.server_chats(), vec![text()]);
    let sealed = r.mid.sent(Dir::ToServer, PacketKind::Sealed);
    let good = sealed.last().expect("a sealed datagram").clone();

    let refused_before = r.server.totals();
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x01;
        r.mid.inject(Dir::ToServer, &bad);
    }
    for n in 0..good.len() {
        r.mid.inject(Dir::ToServer, &good[..n]);
    }
    r.steps(5);
    let after = r.server.totals();
    let refused = (after.auth_failed - refused_before.auth_failed)
        + (after.malformed - refused_before.malformed)
        + (after.foreign - refused_before.foreign)
        + (after.replayed - refused_before.replayed);
    assert_eq!(refused, good.len() as u64 * 2, "every forgery is counted once");
    assert_eq!(r.server.session().len(), 1, "tampering must not end the session");
    assert!(r.server_chats().is_empty(), "nothing tampered was delivered");
    assert!(r.client.is_connected());
    r.chat("still here");
    r.steps(20);
    assert_eq!(r.server_chats(), vec!["still here".to_owned()]);
}

#[test]
fn replayed_datagrams_deliver_nothing_twice() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.connect();
    r.chat(&text());
    r.steps(20);
    assert_eq!(r.server_chats(), vec![text()]);
    let to_server = r.mid.sent(Dir::ToServer, PacketKind::Sealed);
    let to_client = r.mid.sent(Dir::ToClient, PacketKind::Sealed);
    assert!(!to_server.is_empty() && !to_client.is_empty());
    let (replayed_s, replayed_c) = (r.server.totals().replayed, r.client.totals().replayed);
    for d in &to_server {
        r.mid.inject(Dir::ToServer, d);
    }
    for d in &to_client {
        r.mid.inject(Dir::ToClient, d);
    }
    r.steps(2);
    assert!(r.server.totals().replayed - replayed_s >= to_server.len() as u64);
    assert!(r.client.totals().replayed - replayed_c >= to_client.len() as u64);
    assert!(r.server_chats().is_empty(), "no duplicate message");
    assert_eq!(r.server.session().len(), 1);
}

#[test]
fn plain_traffic_on_a_sealed_link_is_refused_though_the_token_is_right() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.connect();
    r.steps(5);
    // The token is on the wire in the clear, so an attacker has it.
    let sealed = r.mid.sent(Dir::ToServer, PacketKind::Sealed);
    let (_, body) = split_datagram(sealed.last().expect("sealed")).expect("framing");
    let token = u32::from_le_bytes(body[..4].try_into().expect("token"));

    let mut packet = connected_header(PacketKind::Payload, token).to_vec();
    packet.extend_from_slice(&[0u8; 9]);
    let mut goodbye = connected_header(PacketKind::Disconnect, token).to_vec();
    goodbye.push(DisconnectReason::Kicked.to_u8());

    let (s, c) = (r.server.totals().auth_failed, r.client.totals().auth_failed);
    for dir in [Dir::ToServer, Dir::ToClient] {
        r.mid.inject(dir, &packet);
        r.mid.inject(dir, &goodbye);
    }
    r.steps(3);
    assert_eq!(r.server.totals().auth_failed - s, 2);
    assert_eq!(r.client.totals().auth_failed - c, 2);
    assert_eq!(
        r.server.session().len(),
        1,
        "a plain goodbye must not drop the peer"
    );
    assert!(r.client.is_connected(), "a plain goodbye must not end the client");
}

#[test]
fn another_sessions_keys_are_refused() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.connect();
    r.steps(5);
    let sealed = r.mid.sent(Dir::ToServer, PacketKind::Sealed);
    let (_, body) = split_datagram(sealed.last().expect("sealed")).expect("framing");
    let token = u32::from_le_bytes(body[..4].try_into().expect("token"));

    // The attacker runs a complete handshake of its own and seals under the victim's token.
    let identity = ServerIdentity::generate().expect("random");
    let mut handshake = ClientHandshake::start(&[3; 32], None).expect("start");
    let (mut own, reply) = accept_handshake(&identity, &[3; 32], handshake.message(), token).expect("accept");
    handshake.read_reply(&reply).expect("reply");
    // Counters the victim has not used, so only the key can stop this: a low counter would be
    // refused as a replay before any decryption.
    let mut forged = Vec::new();
    for _ in 0..1000 {
        forged = own.seal(SealedKind::Disconnect, &[1]).expect("seal");
    }

    let before = r.server.totals().auth_failed;
    r.mid.inject(Dir::ToServer, &forged);
    r.steps(2);
    assert_eq!(r.server.totals().auth_failed, before + 1);
    assert_eq!(r.server.session().len(), 1);
}

#[test]
fn a_wrong_pin_ends_before_the_join_token_leaves() {
    let wrong = ServerKey([0x5A; 32]);
    let mut r = rig(
        |_| {},
        |c| {
            c.server_key = Some(wrong);
            c.password = Some(text());
        },
        forward(),
    );
    assert_eq!(r.run_to_end(), ClientEnd::ServerKeyMismatch);
    assert_eq!(
        r.client.status().crypto.server_key,
        None,
        "an unproven key is not reported"
    );
    assert!(
        r.mid.sent(Dir::ToServer, PacketKind::Sealed).is_empty(),
        "no secret left"
    );
    assert_eq!(r.server.session().len(), 0);
}

#[test]
fn the_right_pin_connects_and_is_reported() {
    let identity = ServerIdentity::generate().expect("random");
    let key = identity.public();
    let mut r = rig(
        |s| s.identity = Some(identity),
        |c| c.server_key = Some(key),
        forward(),
    );
    r.connect();
    assert!(r.client.is_sealed());
    assert_eq!(r.client.server_key(), Some(key));
    assert_eq!(r.server.server_key(), Some(key));
    assert!(r.client.status().crypto.pinned);
}

#[test]
fn a_pin_forces_a_seal_on_loopback_too() {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let identity = ServerIdentity::generate().expect("random");
    let key = identity.public();
    let mut server: NetServer<Duel, _> = NetServer::new(
        net.bind(server_addr).expect("bind"),
        ServerConfig {
            identity: Some(identity),
            ..ServerConfig::default()
        },
    );
    let mut client: NetClient<Duel, _> = NetClient::connect(
        net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
        server_addr,
        ClientConfig {
            server_key: Some(key),
            ..ClientConfig::default()
        },
        &Team(0),
    )
    .expect("fits");
    for step in 0..100 {
        client.update(step * STEP_MS);
        server.update(step * STEP_MS);
    }
    assert!(client.is_connected());
    assert!(
        client.is_sealed(),
        "a pin that is never checked would be worthless"
    );
}

#[test]
fn an_attacker_in_the_middle_is_caught_by_a_pin() {
    // The box answers the key exchange itself with its own static key, as an active attacker
    // would, with a correct prologue: only the pin can tell.
    let attacker = ServerIdentity::generate().expect("random");
    let key = ServerIdentity::generate().expect("random").public();
    let forged: Rc<RefCell<Vec<Vec<u8>>>> = Rc::default();
    let queue = forged.clone();
    let mut seen = None;
    let rule: Rule = Box::new(move |dir, bytes| {
        let Some((kind, body)) = split_datagram(bytes) else {
            return vec![bytes.to_vec()];
        };
        match (dir, kind) {
            (Dir::ToClient, PacketKind::Challenge) => {
                let ch = Challenge::decode(body).expect("challenge");
                seen = Some((ch.client_nonce, ch.cookie));
                vec![bytes.to_vec()]
            }
            (Dir::ToServer, PacketKind::KeyInit) => {
                let init = KeyInit::decode(body).expect("init");
                let (client_nonce, cookie) = seen.expect("the challenge came first");
                let hash = prologue(protocol_id::<Duel>(), client_nonce, init.version, &cookie, None);
                let (_, noise) = accept_handshake(&attacker, &hash, &init.noise, 44).expect("accept");
                let reply = KeyReply { client_nonce, noise };
                queue.borrow_mut().push(reply.to_datagram());
                Vec::new()
            }
            _ => vec![bytes.to_vec()],
        }
    });
    let mut r = rig(|_| {}, |c| c.server_key = Some(key), rule);
    for _ in 0..200 {
        r.step();
        for d in forged.borrow_mut().drain(..) {
            r.mid.inject(Dir::ToClient, &d);
        }
        if r.ended().is_some() {
            break;
        }
    }
    assert_eq!(r.ended(), Some(&ClientEnd::ServerKeyMismatch));
    assert_eq!(
        r.server.session().len(),
        0,
        "the real server never heard from the client"
    );
}

#[test]
fn version_and_mode_mismatches_refuse_with_their_own_codes() {
    // A client that will not encrypt, a remote server that requires it.
    let mut r = rig(|_| {}, |c| c.crypto = CryptoMode::Off, forward());
    assert_eq!(r.run_to_end().as_str(), "encryption_required");
    // A client that requires it, a server that will not. The server's plain challenge is
    // unauthenticated, so the client counts it and the attempt ends on the connect timeout.
    let mut r = rig(|s| s.crypto = CryptoMode::Off, |_| {}, forward());
    assert_eq!(r.run_to_end().as_str(), "connect_timeout");
    assert!(!r.client.is_connected());
    assert!(!r.client.is_sealed());
    assert!(r.client.totals().auth_failed >= 1);
    assert_eq!(r.server.session().len(), 0);
    // Both off: a plain link is the owner's explicit choice.
    let mut r = rig(
        |s| s.crypto = CryptoMode::Off,
        |c| c.crypto = CryptoMode::Off,
        forward(),
    );
    r.connect();
    assert!(!r.client.is_sealed());
}

#[test]
fn a_different_suite_version_is_refused_by_the_server() {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(SERVER_ADDR);
    let mut server: NetServer<Duel, _> = NetServer::with_secret(
        net.bind(server_addr).expect("bind"),
        ServerConfig::default(),
        [7; 32],
    );
    let mut raw = net.bind(SocketAddr::from(CLIENT_ADDR)).expect("bind");
    let request = ConnectRequest {
        protocol_id: protocol_id::<Duel>(),
        client_nonce: 5,
        crypto_version: CRYPTO_VERSION + 1,
        crypto_required: true,
    };
    raw.send_to(&request.to_datagram(), server_addr).expect("send");
    server.update(10);
    let mut buf = [0u8; 2048];
    let (n, _) = raw.recv_from(&mut buf).expect("io").expect("reply");
    let (kind, body) = split_datagram(&buf[..n]).expect("framing");
    assert_eq!(kind, PacketKind::Deny);
    let deny = Deny::decode(body).expect("deny");
    assert_eq!(deny.reason.as_str(), "crypto_version_mismatch");
    assert_eq!(deny.client_nonce, 5);
}

#[test]
fn a_different_suite_version_is_refused_by_the_client() {
    let rule: Rule = Box::new(|dir, bytes| {
        if dir == Dir::ToClient
            && let Some((PacketKind::Challenge, body)) = split_datagram(bytes)
        {
            let mut ch = Challenge::decode(body).expect("challenge");
            ch.crypto_version = CRYPTO_VERSION + 1;
            return vec![ch.to_datagram()];
        }
        vec![bytes.to_vec()]
    });
    let mut r = rig(|_| {}, |_| {}, rule);
    assert_eq!(r.run_to_end().as_str(), "crypto_version_mismatch");
}

#[test]
fn the_wire_version_keeps_old_builds_out() {
    assert_eq!(
        regia_net::REGIA_NET_WIRE_VERSION,
        6,
        "2 with the sealed record, 3 with the reliable input lane (ADR-7896), 4 with the input feedback (ADR-8077), \
         5 with transport v2 (ADR-8184) and the resume ticket (ADR-8298), 6 with the resumption ticket and the \
         one-round-trip resume (ADR-8858)"
    );
}

#[test]
fn stripping_the_crypto_choice_cannot_downgrade_the_link() {
    let rule: Rule = Box::new(|dir, bytes| {
        if dir == Dir::ToClient
            && let Some((PacketKind::Challenge, body)) = split_datagram(bytes)
        {
            let mut ch = Challenge::decode(body).expect("challenge");
            ch.crypto_version = 0;
            return vec![ch.to_datagram()];
        }
        vec![bytes.to_vec()]
    });
    let mut r = rig(|_| {}, |c| c.password = Some(text()), rule);
    assert_eq!(r.run_to_end().as_str(), "connect_timeout");
    assert!(
        r.client.totals().auth_failed >= 1,
        "the rewritten challenge was counted, not joined"
    );
    assert_eq!(r.server.session().len(), 0);
    assert!(
        r.mid
            .sent(Dir::ToServer, PacketKind::ChallengeResponse)
            .is_empty(),
        "no plain join"
    );
}

/// A plain `Challenge` is unauthenticated: anyone who read the cleartext `ConnectRequest` nonce
/// can write one. On a link that must be sealed it is counted and ignored, so the genuine
/// `CRYPTO_VERSION` challenge that follows still completes the handshake.
#[test]
fn a_forged_plain_challenge_does_not_abort_a_required_handshake() {
    let forged = Rc::new(RefCell::new(Vec::new()));
    let held = Rc::new(RefCell::new(None));
    let held_once = Rc::new(Cell::new(false));
    let rule = {
        let forged = forged.clone();
        let held = held.clone();
        let held_once = held_once.clone();
        Box::new(move |dir: Dir, bytes: &[u8]| match split_datagram(bytes) {
            Some((PacketKind::ConnectRequest, body)) if dir == Dir::ToServer => {
                let client_nonce = ConnectRequest::decode(body).expect("request").client_nonce;
                let plain = Challenge {
                    client_nonce,
                    cookie: [9; 16],
                    password: None,
                    crypto_version: PLAIN_VERSION,
                };
                forged.borrow_mut().push(plain.to_datagram());
                vec![bytes.to_vec()]
            }
            // Deliver the forgery before the server's real challenge.
            Some((PacketKind::Challenge, _)) if dir == Dir::ToClient && !held_once.get() => {
                held_once.set(true);
                *held.borrow_mut() = Some(bytes.to_vec());
                Vec::new()
            }
            _ => vec![bytes.to_vec()],
        }) as Rule
    };
    let mut r = rig(|_| {}, |c| c.crypto = CryptoMode::Required, rule);
    for _ in 0..200 {
        r.step();
        for d in forged.borrow_mut().drain(..) {
            r.mid.inject(Dir::ToClient, &d);
        }
        if let Some(ch) = held.borrow_mut().take() {
            r.mid.inject(Dir::ToClient, &ch);
        }
        if r.client.is_connected() {
            break;
        }
    }
    assert!(
        r.client.is_connected(),
        "the real challenge must still connect the client: {:?}",
        r.client.state()
    );
    assert!(
        r.client.is_sealed(),
        "only the sealed suite may finish the handshake"
    );
    assert!(
        r.client.totals().auth_failed >= 1,
        "the plain challenge was counted"
    );
    assert_eq!(r.server.session().len(), 1);
}

#[test]
fn rewriting_the_request_cannot_downgrade_the_link_either() {
    // The box clears the offer. The remote server requires a seal, so it refuses.
    let rule: Rule = Box::new(|dir, bytes| {
        if dir == Dir::ToServer
            && let Some((PacketKind::ConnectRequest, body)) = split_datagram(bytes)
        {
            let mut req = ConnectRequest::decode(body).expect("request");
            req.crypto_version = 0;
            req.crypto_required = false;
            return vec![req.to_datagram()];
        }
        vec![bytes.to_vec()]
    });
    let mut r = rig(|_| {}, |_| {}, rule);
    assert_eq!(r.run_to_end().as_str(), "encryption_required");
}

#[test]
fn a_changed_password_challenge_fails_the_handshake() {
    let rule: Rule = Box::new(|dir, bytes| {
        if dir == Dir::ToClient
            && let Some((PacketKind::Challenge, body)) = split_datagram(bytes)
        {
            let mut ch = Challenge::decode(body).expect("challenge");
            if let Some(p) = ch.password.as_mut() {
                p.salt[0] ^= 1;
            }
            return vec![ch.to_datagram()];
        }
        vec![bytes.to_vec()]
    });
    let mut r = rig(
        |s| s.password = Some("sesame".into()),
        |c| {
            c.password = Some("sesame".into());
            c.connect_timeout_ms = 2_000;
        },
        rule,
    );
    assert_eq!(r.run_to_end(), ClientEnd::ConnectTimeout);
    assert!(
        r.client.totals().auth_failed > 0,
        "the reply did not authenticate"
    );
    assert!(
        r.mid.sent(Dir::ToServer, PacketKind::Sealed).is_empty(),
        "no proof left"
    );
}

#[test]
fn a_forged_key_reply_does_not_end_the_attempt() {
    let mut sent_forgery = false;
    let rule: Rule = Box::new(move |dir, bytes| {
        if dir == Dir::ToClient
            && !sent_forgery
            && let Some((PacketKind::KeyReply, _)) = split_datagram(bytes)
        {
            sent_forgery = true;
            let mut forged = bytes.to_vec();
            let last = forged.len() - 1;
            forged[last] ^= 1;
            return vec![forged, bytes.to_vec()];
        }
        vec![bytes.to_vec()]
    });
    let mut r = rig(|_| {}, |_| {}, rule);
    r.connect();
    assert!(r.client.is_sealed());
    assert_eq!(r.client.totals().auth_failed, 1);
}

#[test]
fn a_plain_accept_cannot_finish_a_sealed_handshake() {
    let mut forged_once = false;
    let rule: Rule = Box::new(move |dir, bytes| {
        if dir == Dir::ToClient
            && !forged_once
            && let Some((PacketKind::KeyReply, body)) = split_datagram(bytes)
        {
            forged_once = true;
            let nonce = u64::from_le_bytes(body[..8].try_into().expect("nonce"));
            let accept = regia_net::handshake::Accept {
                client_nonce: nonce,
                session_token: 9,
                peer_id: 3,
                tick_rate_hz: 30,
                server_tick: 0,
                resume_ticket: [0; 16],
                resumption: None,
            };
            return vec![bytes.to_vec(), accept.to_datagram()];
        }
        vec![bytes.to_vec()]
    });
    let mut r = rig(|_| {}, |_| {}, rule);
    r.connect();
    assert_eq!(
        r.client.peer_id(),
        r.server.session().peers().next().map(|p| p.id)
    );
    assert_eq!(r.client.totals().auth_failed, 1, "the plain Accept was refused");
}

/// Forges a plain `Accept` for the nonce it reads in the `ConnectRequest`, and holds back the
/// server's answers for `hold` datagrams, so the forgery lands while the client has no keys.
fn accept_before_the_challenge(forged: Rc<RefCell<Vec<Vec<u8>>>>, mut hold: usize) -> Rule {
    Box::new(move |dir, bytes| match split_datagram(bytes) {
        Some((PacketKind::ConnectRequest, body)) if dir == Dir::ToServer => {
            let client_nonce = ConnectRequest::decode(body).expect("request").client_nonce;
            let accept = regia_net::handshake::Accept {
                client_nonce,
                session_token: 9,
                peer_id: 3,
                tick_rate_hz: 30,
                server_tick: 0,
                resume_ticket: [0; 16],
                resumption: None,
            };
            forged.borrow_mut().push(accept.to_datagram());
            vec![bytes.to_vec()]
        }
        Some((PacketKind::Challenge | PacketKind::Deny, _)) if hold > 0 => {
            hold -= 1;
            Vec::new()
        }
        _ => vec![bytes.to_vec()],
    })
}

/// Drops the client's answer to the `Challenge` and forges the plain `Accept` for its nonce, so the
/// server never hears of the client.
fn accept_instead_of_the_server(forged: Rc<RefCell<Vec<Vec<u8>>>>) -> Rule {
    Box::new(move |dir, bytes| match split_datagram(bytes) {
        Some((PacketKind::ChallengeResponse, body)) if dir == Dir::ToServer => {
            let client_nonce = ChallengeResponse::decode(body).expect("response").client_nonce;
            let accept = regia_net::handshake::Accept {
                client_nonce,
                session_token: 9,
                peer_id: 3,
                tick_rate_hz: 30,
                server_tick: 0,
                resume_ticket: [0; 16],
                resumption: None,
            };
            forged.borrow_mut().push(accept.to_datagram());
            Vec::new()
        }
        _ => vec![bytes.to_vec()],
    })
}

#[test]
fn a_forged_plain_accept_before_the_challenge_cannot_connect_a_required_client() {
    let forged = Rc::new(RefCell::new(Vec::new()));
    let rule = accept_before_the_challenge(forged.clone(), 3);
    let identity = ServerIdentity::generate().expect("random");
    let key = identity.public();
    let mut r = rig(
        |s| s.identity = Some(identity),
        |c| c.server_key = Some(key),
        rule,
    );
    for _ in 0..200 {
        r.step();
        for d in forged.borrow_mut().drain(..) {
            r.mid.inject(Dir::ToClient, &d);
        }
        if r.client.is_connected() {
            break;
        }
    }
    assert!(r.client.is_connected(), "{:?}", r.client.state());
    assert!(r.client.is_sealed(), "only the sealed Accept may connect it");
    assert_eq!(r.client.server_key(), Some(key));
    assert!(r.client.totals().auth_failed >= 1, "the forgery reached it");
    assert_eq!(r.server.session().len(), 1);
}

#[test]
fn the_same_forgery_cannot_connect_a_client_that_turned_encryption_off() {
    // This was the control (the attack landed on a plain client). Since ADR-1957 a plain client
    // also takes an `Accept` only after a `Challenge`, so the forgery fails there too (ADR-1975).
    let forged = Rc::new(RefCell::new(Vec::new()));
    let rule = accept_before_the_challenge(forged.clone(), usize::MAX);
    let mut r = rig(|_| {}, |c| c.crypto = CryptoMode::Off, rule);
    for _ in 0..20 {
        r.step();
        for d in forged.borrow_mut().drain(..) {
            r.mid.inject(Dir::ToClient, &d);
        }
    }
    assert!(!r.client.is_connected(), "{:?}", r.client.state());
    assert!(!r.client.is_sealed());
    assert!(r.client.totals().malformed >= 1, "the forgery reached it");
    assert_eq!(r.server.session().len(), 0, "the server never heard of it");
}

#[test]
fn the_same_forgery_connects_a_client_that_turned_encryption_off() {
    // The control: it shows a forged plain Accept lands when nothing refuses it. A plain client
    // takes no Accept before a Challenge (`an_accept_before_any_challenge_is_not_taken`), so this
    // forgery comes after one, and the client's answer never reaches the server. Both sides chose
    // a plain link, so nothing in it is secret: an on-path forger reads the nonce and wins. That is
    // why the defaults require encryption off loopback (`the_defaults_refuse_a_plain_remote_peer`)
    // and why a required side refuses a plain one (`version_and_mode_mismatches_refuse_with_their_own_codes`).
    // ADR-2015: the integ/v031 merge had given this control the asserts of the test above.
    let forged = Rc::new(RefCell::new(Vec::new()));
    let rule = accept_instead_of_the_server(forged.clone());
    let mut r = rig(
        |s| s.crypto = CryptoMode::Off,
        |c| c.crypto = CryptoMode::Off,
        rule,
    );
    for _ in 0..20 {
        r.step();
        for d in forged.borrow_mut().drain(..) {
            r.mid.inject(Dir::ToClient, &d);
        }
    }
    assert!(r.client.is_connected(), "{:?}", r.client.state());
    assert!(!r.client.is_sealed());
    assert_eq!(r.server.session().len(), 0, "the server never heard of it");
}

#[test]
fn the_defaults_refuse_a_plain_remote_peer() {
    // The guard behind the control above (ADR-2015): every place a game or a dedicated server
    // gets its encryption mode from defaults to `Auto`, which requires encryption for every
    // non-loopback peer. A merge that changes one of these defaults fails here by name.
    assert_eq!(CryptoMode::default(), CryptoMode::Auto);
    assert_eq!(ServerConfig::default().crypto, CryptoMode::Auto, "NetServer");
    assert_eq!(ClientConfig::default().crypto, CryptoMode::Auto, "NetClient");
    assert_eq!(
        SessionSettings::default().encryption,
        CryptoMode::Auto,
        "NetSession"
    );
    assert_eq!(
        regia_net::server_host::ServerSettings::default().encryption,
        CryptoMode::Auto,
        "the dedicated server's settings"
    );
    let remote = SocketAddr::from(([203, 0, 113, 7], 7777));
    let loopback = SocketAddr::from(([127, 0, 0, 1], 7777));
    assert!(CryptoMode::Auto.requires(remote));
    assert!(!CryptoMode::Auto.requires(loopback));
    assert!(CryptoMode::Required.requires(loopback));
    assert!(!CryptoMode::Off.requires(remote));
    // End to end: a client that turned encryption off is refused by a default server, which
    // never adds it.
    let mut r = rig(|_| {}, |c| c.crypto = CryptoMode::Off, forward());
    assert_eq!(r.run_to_end().as_str(), "encryption_required");
    assert!(!r.client.is_connected());
    assert_eq!(r.server.session().len(), 0);
}

/// Runs an attacker's client next to a live victim, on the victim's address.
fn spoof_next_to_a_live_peer(r: &mut Rig, password: Option<&str>) {
    r.connect();
    let peer = r.peer();
    while r.server.poll_event().is_some() {}
    let mut mallory = r.spoofer(password);
    for _ in 0..200 {
        mallory.update(r.now);
        r.step();
    }
    assert!(!mallory.is_connected(), "{:?}", mallory.state());
    assert!(r.client.is_connected(), "{:?}", r.client.state());
    assert_eq!(r.server.session().len(), 1);
    assert_eq!(r.peer(), peer, "the victim keeps its slot");
    r.chat("still here");
    r.steps(10);
    let events: Vec<ServerEvent<Duel>> = std::iter::from_fn(|| r.server.poll_event()).collect();
    assert!(
        !events.iter().any(|e| matches!(e, ServerEvent::PeerLeft { .. })),
        "the victim was never dropped"
    );
    let chats = events.iter().filter(
        |e| matches!(e, ServerEvent::Message { message: DuelMsg::Chat(text), .. } if text == "still here"),
    );
    assert_eq!(chats.count(), 1, "and its traffic still arrives");
}

#[test]
fn a_spoofed_join_cannot_replace_a_live_sealed_peer() {
    let mut r = rig(|_| {}, |_| {}, forward());
    spoof_next_to_a_live_peer(&mut r, None);
    assert!(r.server.totals().handshakes_refused >= 1);
}

#[test]
fn a_spoofed_join_with_a_wrong_password_costs_the_victim_nothing() {
    let mut r = rig(
        |s| s.password = Some("sesame".into()),
        |c| c.password = Some("sesame".into()),
        forward(),
    );
    spoof_next_to_a_live_peer(&mut r, Some("guess"));
}

#[test]
fn a_restarted_client_replaces_its_old_peer_once_that_peer_went_quiet() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.connect();
    let old = r.peer();
    while r.server.poll_event().is_some() {}
    let mut restarted = r.spoofer(None);
    // The old client is gone: nothing drives it any more.
    let mut joined_at = None;
    for _ in 0..400 {
        r.now += STEP_MS;
        restarted.update(r.now);
        r.mid.pump();
        r.server.update(r.now);
        r.mid.pump();
        if restarted.is_connected() {
            joined_at = Some(r.now);
            break;
        }
    }
    let joined_at = joined_at.expect("it joined");
    assert!(
        joined_at >= regia_net::server::SEALED_PEER_QUIET_MS,
        "not before the quiet time: {joined_at}"
    );
    assert!(restarted.is_sealed());
    assert_eq!(r.server.session().len(), 1);
    assert_ne!(r.peer(), old);
    assert!(
        std::iter::from_fn(|| r.server.poll_event())
            .any(|e| matches!(e, ServerEvent::PeerLeft { peer, .. } if peer == old))
    );
}

#[test]
fn one_key_exchange_buys_one_password_guess() {
    // The sealed refusal is lost, so the client keeps resending its join.
    let rule: Rule = Box::new(|dir, bytes| {
        let sealed = split_datagram(bytes).is_some_and(|(k, _)| k == PacketKind::Sealed);
        if dir == Dir::ToClient && sealed {
            Vec::new()
        } else {
            vec![bytes.to_vec()]
        }
    });
    let mut r = rig(
        |s| s.password = Some("sesame".into()),
        |c| c.password = Some("guess".into()),
        rule,
    );
    assert_eq!(r.run_to_end(), ClientEnd::ConnectTimeout);
    let joins = r.mid.sent(Dir::ToServer, PacketKind::Sealed).len();
    assert!(joins >= 3, "the client resent its join: {joins}");
    assert_eq!(r.server.totals().denied, 1, "only the first was checked");
    assert_eq!(r.server.totals().handshakes, 1);
}

#[test]
fn every_lost_handshake_packet_recovers_without_new_curve_work() {
    for lost in [
        PacketKind::KeyInit,
        PacketKind::KeyReply,
        PacketKind::Sealed,
        PacketKind::ConnectRequest,
        PacketKind::Challenge,
    ] {
        for dir in [Dir::ToServer, Dir::ToClient] {
            let mut dropped = false;
            let rule: Rule = Box::new(move |d, bytes| {
                if d == dir && !dropped && split_datagram(bytes).is_some_and(|(k, _)| k == lost) {
                    dropped = true;
                    return Vec::new();
                }
                vec![bytes.to_vec()]
            });
            let mut r = rig(|_| {}, |_| {}, rule);
            r.connect();
            assert!(r.client.is_sealed(), "{lost:?} {dir:?}");
            assert_eq!(
                r.server.totals().handshakes,
                1,
                "{lost:?} {dir:?}: a resend costs no curve work"
            );
        }
    }
}

#[test]
fn a_key_init_without_a_valid_cookie_costs_no_curve_work() {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(SERVER_ADDR);
    let mut server: NetServer<Duel, _> = NetServer::with_secret(
        net.bind(server_addr).expect("bind"),
        ServerConfig::default(),
        [7; 32],
    );
    let mut raw = net.bind(SocketAddr::from(CLIENT_ADDR)).expect("bind");
    for i in 0..50u8 {
        let init = KeyInit {
            client_nonce: u64::from(i),
            cookie: [i; 16],
            version: CRYPTO_VERSION,
            noise: [i; 32],
        };
        let bytes = init.to_datagram();
        assert_eq!(bytes.len(), KEY_INIT_BYTES);
        raw.send_to(&bytes, server_addr).expect("send");
    }
    server.update(10);
    assert_eq!(server.totals().handshakes, 0);
    assert_eq!(server.totals().malformed, 50);
    let mut buf = [0u8; 2048];
    // Spec 120 § 11.4 now requires a bounded refresh instead of silence (ADR-7946).
    // Preserve the original no-curve-work assertions and validate every answer exactly.
    let minter = regia_net::handshake::CookieMinter::new([7; 32]);
    for nonce in 0..50 {
        let (n, from) = raw.recv_from(&mut buf).expect("io").expect("fresh challenge");
        assert_eq!(from, server_addr);
        assert!(n <= KEY_INIT_BYTES, "refresh cannot amplify the request");
        let (kind, body) = split_datagram(&buf[..n]).expect("framed challenge");
        assert_eq!(kind, PacketKind::Challenge);
        let challenge = Challenge::decode(body).expect("challenge");
        assert_eq!(challenge.client_nonce, nonce);
        assert_eq!(challenge.crypto_version, CRYPTO_VERSION);
        assert!(
            minter
                .verify(raw.local_addr().expect("addr"), nonce, &challenge.cookie, 10)
                .is_some()
        );
    }
    assert!(raw.recv_from(&mut buf).expect("io").is_none(), "no extra replies");
    assert!(
        server.session().is_empty(),
        "no peer allocated for an invalid proof"
    );
}

/// Answers a request and returns the challenge, as a raw flood client.
fn challenge(
    raw: &mut LoopbackSocket,
    server: &mut NetServer<Duel, LoopbackSocket>,
    to: SocketAddr,
    nonce: u64,
    now: u64,
) -> Challenge {
    let request = ConnectRequest {
        protocol_id: protocol_id::<Duel>(),
        client_nonce: nonce,
        crypto_version: CRYPTO_VERSION,
        crypto_required: true,
    };
    raw.send_to(&request.to_datagram(), to).expect("send");
    server.update(now);
    let mut buf = [0u8; 2048];
    let (n, _) = raw.recv_from(&mut buf).expect("io").expect("challenge");
    Challenge::decode(split_datagram(&buf[..n]).expect("framing").1).expect("challenge")
}

fn drain(raw: &mut LoopbackSocket) {
    let mut buf = [0u8; 2048];
    while raw.recv_from(&mut buf).expect("io").is_some() {}
}

#[test]
fn a_new_share_on_one_cookie_costs_no_curve_work() {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(SERVER_ADDR);
    let mut server: NetServer<Duel, _> = NetServer::with_secret(
        net.bind(server_addr).expect("bind"),
        ServerConfig::default(),
        [7; 32],
    );
    let mut raw = net.bind(SocketAddr::from(CLIENT_ADDR)).expect("bind");
    let ch = challenge(&mut raw, &mut server, server_addr, 1, 1_000);
    drain(&mut raw);
    for i in 0..10u8 {
        let hs = ClientHandshake::start(&[i; 32], None).expect("start");
        let init = KeyInit {
            client_nonce: 1,
            cookie: ch.cookie,
            version: CRYPTO_VERSION,
            noise: *hs.message(),
        };
        raw.send_to(&init.to_datagram(), server_addr).expect("send");
    }
    server.update(1_010);
    assert_eq!(server.totals().handshakes, 1);
    assert_eq!(server.totals().handshakes_refused, 9);
}

#[test]
fn a_flood_of_valid_cookies_is_capped_per_second() {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(SERVER_ADDR);
    let mut server: NetServer<Duel, _> = NetServer::with_secret(
        net.bind(server_addr).expect("bind"),
        ServerConfig {
            max_handshakes_per_s: 3,
            ..ServerConfig::default()
        },
        [7; 32],
    );
    let mut raw = net.bind(SocketAddr::from(CLIENT_ADDR)).expect("bind");
    let cookies: Vec<[u8; 16]> = (0..10u64)
        .map(|nonce| {
            let cookie = challenge(&mut raw, &mut server, server_addr, nonce, 1_000).cookie;
            drain(&mut raw);
            cookie
        })
        .collect();
    for (nonce, cookie) in cookies.into_iter().enumerate() {
        let hs = ClientHandshake::start(&[nonce as u8; 32], None).expect("start");
        let init = KeyInit {
            client_nonce: nonce as u64,
            cookie,
            version: CRYPTO_VERSION,
            noise: *hs.message(),
        };
        raw.send_to(&init.to_datagram(), server_addr).expect("send");
    }
    server.update(1_010);
    assert_eq!(server.totals().handshakes, 3);
    assert_eq!(server.totals().handshakes_refused, 7);
    server.update(2_000);
    assert_eq!(
        server.totals().handshakes,
        3,
        "a new second admits work again, it does not replay any"
    );
}

#[test]
fn one_address_is_held_to_the_per_ip_handshake_limit() {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(SERVER_ADDR);
    let mut server: NetServer<Duel, _> = NetServer::with_secret(
        net.bind(server_addr).expect("bind"),
        ServerConfig {
            max_handshakes_per_s: 0,
            handshakes_per_ip_per_s: 2,
            ..ServerConfig::default()
        },
        [7; 32],
    );
    let mut raw = net.bind(SocketAddr::from(CLIENT_ADDR)).expect("bind");
    // One cookie per nonce, slowly enough for the request limit.
    let cookies: Vec<[u8; 16]> = (0..10u64)
        .map(|nonce| {
            let cookie = challenge(&mut raw, &mut server, server_addr, nonce, 1_000 + nonce * 500).cookie;
            drain(&mut raw);
            cookie
        })
        .collect();
    for (nonce, cookie) in cookies.into_iter().enumerate() {
        let hs = ClientHandshake::start(&[nonce as u8; 32], None).expect("start");
        let init = KeyInit {
            client_nonce: nonce as u64,
            cookie,
            version: CRYPTO_VERSION,
            noise: *hs.message(),
        };
        raw.send_to(&init.to_datagram(), server_addr).expect("send");
    }
    server.update(6_010);
    assert_eq!(server.totals().handshakes, 4, "the burst, twice the rate");
    assert_eq!(server.totals().handshakes_refused, 6);
    assert_eq!(server.handshakes_refused(), 6);
}

#[test]
fn one_ip_cannot_fill_the_handshake_table() {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(SERVER_ADDR);
    let mut server: NetServer<Duel, _> = NetServer::with_secret(
        net.bind(server_addr).expect("bind"),
        ServerConfig {
            max_handshakes_per_s: 0,
            ..ServerConfig::default()
        },
        [7; 32],
    );
    let limit = regia_net::crypto::MAX_PENDING_PER_IP as u64;
    for port in 0..limit + 3 {
        let mut raw = net
            .bind(SocketAddr::from(([10, 0, 0, 2], 6000 + port as u16)))
            .expect("bind");
        let ch = challenge(&mut raw, &mut server, server_addr, port, 1_000);
        drain(&mut raw);
        let hs = ClientHandshake::start(&[1; 32], None).expect("start");
        let init = KeyInit {
            client_nonce: port,
            cookie: ch.cookie,
            version: CRYPTO_VERSION,
            noise: *hs.message(),
        };
        raw.send_to(&init.to_datagram(), server_addr).expect("send");
        server.update(1_001);
    }
    assert_eq!(server.totals().handshakes, limit);
    assert_eq!(server.totals().handshakes_refused, 3);
}

#[test]
fn right_and_wrong_passwords_over_a_sealed_link() {
    let mut r = rig(
        |s| s.password = Some("sesame".into()),
        |c| c.password = Some("sesame".into()),
        forward(),
    );
    r.connect();
    assert!(r.client.is_sealed());
    let mut r = rig(
        |s| s.password = Some("sesame".into()),
        |c| c.password = Some("guess".into()),
        forward(),
    );
    match r.run_to_end() {
        ClientEnd::Denied { reason, .. } => assert_eq!(reason, DenyReason::BadPassword),
        other => panic!("{other:?}"),
    }
    assert!(
        r.mid.sent(Dir::ToClient, PacketKind::Deny).is_empty(),
        "a refusal after the key exchange travels sealed"
    );
}

/// Audit wave 5 (netplug L2): before any keys exist, a client that requires a seal ignores a
/// plain `Deny` whose reason the server only ever sends sealed (here `bad_password`, forged from
/// the nonce read off the `ConnectRequest`), and still connects sealed. A plain refusal the server
/// does send before the keys (`banned`) still ends the attempt.
#[test]
fn a_required_client_ignores_a_plain_deny_the_server_only_sends_sealed() {
    for (reason, ignored) in [(DenyReason::BadPassword, true), (DenyReason::Banned, false)] {
        let forged = Rc::new(RefCell::new(Vec::new()));
        let sink = forged.clone();
        let rule: Rule = Box::new(move |dir, bytes| {
            if dir == Dir::ToServer
                && let Some((PacketKind::ConnectRequest, body)) = split_datagram(bytes)
            {
                let client_nonce = ConnectRequest::decode(body).expect("request").client_nonce;
                let deny = Deny {
                    client_nonce,
                    reason,
                    detail: "forged".into(),
                };
                sink.borrow_mut().push(deny.to_datagram());
            }
            vec![bytes.to_vec()]
        });
        let identity = ServerIdentity::generate().expect("random");
        let key = identity.public();
        let mut r = rig(
            |s| s.identity = Some(identity),
            |c| c.server_key = Some(key),
            rule,
        );
        for _ in 0..200 {
            r.step();
            for d in forged.borrow_mut().drain(..) {
                r.mid.inject(Dir::ToClient, &d);
            }
            if r.client.is_connected() || r.ended().is_some() {
                break;
            }
        }
        if ignored {
            assert!(r.client.is_connected(), "{reason:?}: {:?}", r.client.state());
            assert!(r.client.is_sealed());
            assert!(r.client.totals().auth_failed >= 1, "the forgery reached it");
        } else {
            match r.ended() {
                Some(ClientEnd::Denied { reason: got, .. }) => assert_eq!(*got, reason),
                other => panic!("{reason:?}: {other:?}"),
            }
        }
    }
}

#[test]
fn a_plain_deny_is_ignored_once_the_link_is_sealed() {
    let mut forged = false;
    let rule: Rule = Box::new(move |dir, bytes| {
        if dir == Dir::ToClient
            && !forged
            && let Some((PacketKind::KeyReply, body)) = split_datagram(bytes)
        {
            forged = true;
            let client_nonce = u64::from_le_bytes(body[..8].try_into().expect("nonce"));
            let deny = Deny {
                client_nonce,
                reason: DenyReason::Banned,
                detail: "forged".into(),
            };
            return vec![bytes.to_vec(), deny.to_datagram()];
        }
        vec![bytes.to_vec()]
    });
    let mut r = rig(|_| {}, |_| {}, rule);
    r.connect();
    assert_eq!(r.client.totals().auth_failed, 1);
}

#[test]
fn reliable_order_survives_wan_stress_on_a_sealed_link() {
    let wan = ConditionerConfig::from_profile("wan_stress").expect("profile");
    let mut r = rig(
        |s| s.conditioner = wan.clone(),
        |c| {
            c.conditioner = wan.clone();
            c.connect_timeout_ms = 60_000;
        },
        forward(),
    );
    for _ in 0..2_000 {
        if r.client.is_connected() {
            break;
        }
        r.step();
    }
    assert!(r.client.is_connected(), "joined under wan_stress");
    let sent: Vec<String> = (0..200).map(|i| format!("message {i}")).collect();
    let mut received = Vec::new();
    for (i, line) in sent.iter().enumerate() {
        r.chat(line);
        r.step();
        if i.is_multiple_of(10) {
            received.extend(r.server_chats());
        }
    }
    for _ in 0..3_000 {
        r.step();
        received.extend(r.server_chats());
        if received.len() == sent.len() {
            break;
        }
    }
    assert_eq!(received, sent, "exactly once, in order");
    assert!(r.client.is_sealed());
}

#[test]
fn large_reliable_messages_fragment_within_the_packet_budget() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.connect();
    // The largest the test game's `Chat` codec takes: several packets on either path.
    let big = "x".repeat(4096);
    r.chat(&big);
    r.steps(100);
    assert_eq!(r.server_chats(), vec![big]);
    let sealed = r.mid.sent(Dir::ToServer, PacketKind::Sealed);
    assert!(
        sealed.iter().filter(|d| d.len() > 1000).count() >= 3,
        "it fragmented"
    );
    let largest = r.mid.log.iter().map(|(_, d)| d.len()).max().expect("traffic");
    let budget = regia_net::LinkConfig::default().max_packet_bytes;
    assert!(largest <= budget, "{largest} bytes on the wire, budget {budget}");
    assert_eq!(SEALED_RESERVED_HEADER_BYTES, 34);
}

#[test]
fn a_server_goodbye_reaches_the_client_sealed() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.connect();
    r.steps(5);
    let peer = r.peer();
    assert!(r.server.kick(peer, DisconnectReason::Kicked));
    assert_eq!(r.run_to_end(), ClientEnd::Disconnected(DisconnectReason::Kicked));
    assert!(
        r.mid.sent(Dir::ToClient, PacketKind::Disconnect).is_empty(),
        "the goodbye is sealed"
    );
}

#[test]
fn a_client_goodbye_reaches_the_server_sealed() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.connect();
    r.steps(5);
    r.client.disconnect();
    r.steps(3);
    assert_eq!(r.server.session().len(), 0);
    assert!(r.mid.sent(Dir::ToServer, PacketKind::Disconnect).is_empty());
}

#[test]
fn a_join_replayed_after_the_accept_gets_the_accept_again_and_nothing_else() {
    let mut drop_accept = true;
    let rule: Rule = Box::new(move |dir, bytes| {
        if dir == Dir::ToClient
            && drop_accept
            && split_datagram(bytes).is_some_and(|(k, _)| k == PacketKind::Sealed)
        {
            drop_accept = false;
            return Vec::new();
        }
        vec![bytes.to_vec()]
    });
    let mut r = rig(|_| {}, |_| {}, rule);
    r.connect();
    assert_eq!(
        r.server.totals().accepted,
        1,
        "one join however many copies arrived"
    );
    assert_eq!(r.server.session().len(), 1);
}

#[test]
fn input_bundles_arrive_the_same_on_plain_and_sealed_links() {
    let run = |mode: CryptoMode| {
        let mut r = rig(|s| s.crypto = mode, |c| c.crypto = mode, forward());
        r.connect();
        let peer = r.peer();
        assert_eq!(r.client.is_sealed(), mode != CryptoMode::Off);
        // Ahead of the server's tick, as a real client stamps it (`NetClient::send_input`).
        for (tick, step) in [(1, 1), (2, -1), (3, 0), (4, 1)] {
            r.client.send_input(tick, Move(step)).expect("queued");
        }
        r.steps(5);
        (0..=4)
            .map(|_| r.server.advance_tick(|_, _| {}).get(&peer).map(|mv| mv.0))
            .collect::<Vec<_>>()
    };
    let expected = vec![None, Some(1), Some(-1), Some(0), Some(1)];
    assert_eq!(run(CryptoMode::Off), expected, "control: the plain path");
    assert_eq!(run(CryptoMode::Auto), expected);
}

#[test]
fn the_unreliable_limit_accounts_for_the_seal_exactly() {
    // `Chat` adds four bytes: lane, tag and a 16-bit length.
    let limit = |mode: CryptoMode| {
        let mut r = rig(|s| s.crypto = mode, |c| c.crypto = mode, forward());
        r.connect();
        let budget = regia_net::LinkConfig::default().max_packet_bytes;
        let mut largest_ok = 0;
        for len in (1000..budget).rev() {
            if r.client
                .send(Channel::Unreliable, &DuelMsg::Chat("x".repeat(len)))
                .is_ok()
            {
                largest_ok = len;
                break;
            }
        }
        r.steps(5);
        assert_eq!(r.server_chats(), vec!["x".repeat(largest_ok)], "{mode:?}");
        let largest = r
            .mid
            .log
            .iter()
            .map(|(_, d)| d)
            .max_by_key(|d| d.len())
            .expect("traffic");
        // Wire 5 (ADR-8184): the transport reserves `PACKET_HEADER_BYTES` for its own header
        // (flags, packet number, a small acknowledgment frame); what that packet's header did
        // not use is the only slack. On the plain path the header can be read back, so the fill
        // is checked exactly; the sealed path's exactness is the 25-byte difference below.
        if mode == CryptoMode::Off {
            let (_, body) = split_datagram(largest).expect("framing");
            let conn = &body[4..];
            let mut rd = regia_net::wire::WireReader::new(conn);
            let flags = rd.u8().expect("flags");
            rd.take([1, 2, 4, 4][usize::from(flags & 0x03)]).expect("number");
            if flags & 0x04 != 0 {
                regia_net::ack::AckFrame::decode(&mut rd, flags & 0x08 != 0).expect("frame");
            }
            let header = rd.position();
            assert_eq!(
                largest.len() + regia_net::connection::PACKET_HEADER_BYTES - header,
                budget,
                "{mode:?}: the largest message fills the packet but for the unused header reserve"
            );
        } else {
            assert!(largest.len() <= budget, "{mode:?}");
        }
        largest_ok
    };
    assert_eq!(
        limit(CryptoMode::Off) - limit(CryptoMode::Auto),
        25,
        "the seal costs 25 bytes"
    );
}

const PASSWORD: &str = "correct horse battery staple";
const ADMIN_TOKEN: &str = "s3cret admin token value";

struct SessionRig {
    server: NetSession,
    client: NetSession,
    mid: Middlebox,
}

/// A server and a client session through the box, on loopback addresses so both modes can carry
/// secrets: the mode is the only thing that differs between a run and its control.
fn session_rig(mode: CryptoMode) -> SessionRig {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let box_addr = SocketAddr::from(([127, 0, 0, 9], 7000));
    let client_addr = SocketAddr::from(([127, 0, 0, 2], 5000));
    let settings = SessionSettings {
        encryption: mode,
        ..SessionSettings::default()
    };
    let manifest = ContentManifest::build("game", "1", 1, 0, &[]);
    let mut server = NetSession::new(settings.clone(), manifest.clone());
    server.set_manual_clock(0);
    server
        .host_on(
            net.bind(server_addr).expect("bind"),
            HostOptions {
                password: PASSWORD.into(),
                admin_token: Some(ADMIN_TOKEN.into()),
                allow_plain_admin: true,
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
    let mut client = NetSession::new(settings, manifest);
    client.set_manual_clock(0);
    client
        .connect_on(
            net.bind(client_addr).expect("bind"),
            box_addr,
            ConnectOptions {
                password: Some(PASSWORD.into()),
                ..ConnectOptions::default()
            },
        )
        .expect("connect");
    let mid = Middlebox {
        socket: net.bind(box_addr).expect("bind"),
        server: server_addr,
        client: client_addr,
        reader: None,
        rule: forward(),
        log: Vec::new(),
    };
    SessionRig { server, client, mid }
}

impl SessionRig {
    fn step(&mut self) {
        self.client.advance_manual_clock(STEP_MS);
        self.server.advance_manual_clock(STEP_MS);
        self.client.update();
        self.mid.pump();
        self.server.update();
        self.mid.pump();
    }

    fn join(&mut self) {
        for _ in 0..1000 {
            if self.client.state().as_str() == "in_game" {
                return;
            }
            self.step();
        }
        panic!("no join: {:?}", self.client.state());
    }
}

#[test]
fn hello_password_and_admin_token_never_cross_in_the_clear() {
    let run = |mode: CryptoMode| {
        let mut r = session_rig(mode);
        r.join();
        r.client.send_admin(SessionMsg::AdminLogin {
            token: ADMIN_TOKEN.into(),
        });
        for _ in 0..50 {
            r.step();
        }
        let logged_in = r.client.take_client_events().iter().any(|e| {
            matches!(e, ClientSessionEvent::AdminOutput(l) if l.first().is_some_and(|t| t == "admin: logged in"))
        });
        assert!(logged_in, "the token reached the server in mode {mode:?}");
        (
            r.mid.wire_contains(PASSWORD.as_bytes()),
            r.mid.wire_contains(ADMIN_TOKEN.as_bytes()),
        )
    };
    assert_eq!(
        run(CryptoMode::Off),
        (true, true),
        "control: the box can read a plain link"
    );
    assert_eq!(run(CryptoMode::Required), (false, false));
}

#[test]
fn secrets_are_refused_on_a_plain_remote_link() {
    let off = SessionSettings {
        encryption: CryptoMode::Off,
        ..SessionSettings::default()
    };
    let manifest = ContentManifest::build("game", "1", 1, 0, &[]);
    let net = LoopbackNet::new();
    let remote = SocketAddr::from(SERVER_ADDR);

    let mut client = NetSession::new(off.clone(), manifest.clone());
    let err = client
        .connect_on(
            net.bind(SocketAddr::from(CLIENT_ADDR)).expect("bind"),
            remote,
            ConnectOptions {
                password: Some(PASSWORD.into()),
                ..ConnectOptions::default()
            },
        )
        .expect_err("a password must not leave on a plain remote link");
    assert_eq!(err, SessionError::PasswordNeedsEncryption);
    assert_eq!(err.code(), "password_needs_encryption");

    let mut host = NetSession::new(off.clone(), manifest.clone());
    let opts = HostOptions {
        password: PASSWORD.into(),
        ..HostOptions::default()
    };
    assert_eq!(
        host.host_on(net.bind(remote).expect("bind"), opts, true),
        Err(SessionError::PasswordNeedsEncryption)
    );
    let opts = HostOptions {
        admin_token: Some(ADMIN_TOKEN.into()),
        ..HostOptions::default()
    };
    let second = SocketAddr::from(([10, 0, 0, 3], 7777));
    assert_eq!(
        host.host_on(net.bind(second).expect("bind"), opts, true),
        Err(SessionError::PasswordNeedsEncryption)
    );

    // An empty password is no secret, and the same secret is fine with encryption on.
    let mut open = NetSession::new(off, manifest.clone());
    open.connect_on(
        net.bind(SocketAddr::from(([10, 0, 0, 4], 5000))).expect("bind"),
        remote,
        ConnectOptions {
            password: Some(String::new()),
            ..ConnectOptions::default()
        },
    )
    .expect("nothing secret");
    let mut sealed = NetSession::new(SessionSettings::default(), manifest);
    sealed
        .connect_on(
            net.bind(SocketAddr::from(([10, 0, 0, 5], 5000))).expect("bind"),
            remote,
            ConnectOptions {
                password: Some(PASSWORD.into()),
                ..ConnectOptions::default()
            },
        )
        .expect("a sealed link may carry it");
}

#[test]
fn a_session_over_a_sealed_link_reports_the_key_on_both_sides() {
    let mut r = session_rig(CryptoMode::Required);
    r.join();
    assert!(r.client.link_sealed());
    let key = r.server.server_key().expect("server key");
    assert_eq!(r.client.server_key(), Some(key));
}

#[test]
fn the_codec_of_a_chat_is_what_the_test_thinks_it_is() {
    // Guards the marker tests: the marker must appear in the plain encoding of a message.
    let bytes = DuelMsg::Chat(text()).to_bytes().expect("chat");
    assert!(bytes.windows(MARKER.len()).any(|w| w == MARKER));
}

// A long client frame (ADR-4929): a cold shader compile on a software rasterizer took 18.8 s
// before the windowed net-game client's second update (25.4 s in a two-machine match), and that
// update threw away the server's waiting challenge as a `connect_timeout`. ADR-3075's stall credit
// keeps the timeout from firing, but past `COOKIE_BUCKET_MS` the waiting challenge's cookie (and
// past `PENDING_TTL_MS` a half-open key exchange) is gone on the server, so the client has to start
// over. The server and the network keep running while the client is stalled.

impl Rig {
    /// The client runs no update for `ms` while the server and the network keep going.
    fn stall_client(&mut self, ms: u64) {
        for _ in 0..ms / STEP_MS {
            self.now += STEP_MS;
            self.mid.pump();
            self.server.update(self.now);
            self.mid.pump();
        }
    }

    /// One client update, then `frame_ms` more of server and network time: a client whose
    /// every frame takes `frame_ms`.
    fn slow_frame(&mut self, frame_ms: u64) {
        self.step();
        self.stall_client(frame_ms);
    }

    /// The distinct client nonces of every connect request on the wire: one per attempt.
    fn attempts(&self) -> usize {
        let mut nonces: Vec<u64> = self
            .mid
            .sent(Dir::ToServer, PacketKind::ConnectRequest)
            .iter()
            .filter_map(|bytes| {
                let (_, body) = split_datagram(bytes)?;
                ConnectRequest::decode(body).ok().map(|r| r.client_nonce)
            })
            .collect();
        nonces.sort_unstable();
        nonces.dedup();
        nonces.len()
    }
}

#[test]
fn a_challenge_that_waited_out_a_long_client_frame_still_connects() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.step();
    assert_eq!(r.mid.sent(Dir::ToServer, PacketKind::ConnectRequest).len(), 1);
    // Two cookie buckets: whatever the server clock's phase, the waiting cookie has expired.
    r.stall_client(25_400);
    assert_eq!(
        r.mid.sent(Dir::ToClient, PacketKind::Challenge).len(),
        1,
        "the server answered while the client was stalled"
    );
    r.connect();
    assert!(r.client.is_sealed());
    assert_eq!(r.server.session().len(), 1);
}

#[test]
fn a_long_client_frame_after_the_key_exchange_starts_the_handshake_over() {
    // A connect timeout far past the stall isolates the expired half-open handshake: before the
    // fix the client resent a join the server had forgotten until it gave up.
    let mut r = rig(|_| {}, |c| c.connect_timeout_ms = 120_000, forward());
    for _ in 0..100 {
        if !r.mid.sent(Dir::ToServer, PacketKind::KeyInit).is_empty() {
            break;
        }
        r.step();
    }
    assert_eq!(r.mid.sent(Dir::ToServer, PacketKind::KeyInit).len(), 1);
    assert!(!r.client.is_connected());
    r.stall_client(regia_net::crypto::PENDING_TTL_MS + 2_000);
    r.connect();
    assert!(r.client.is_sealed());
    assert_eq!(r.server.session().len(), 1);
    assert_eq!(r.attempts(), 2, "the stale attempt was started over once");
    assert_eq!(r.server.totals().handshakes, 2);
}

#[test]
fn a_client_whose_every_frame_takes_three_seconds_connects() {
    let mut r = rig(|_| {}, |_| {}, forward());
    for _ in 0..20 {
        if r.client.is_connected() {
            break;
        }
        r.slow_frame(3_000);
    }
    assert!(
        r.client.is_connected(),
        "slow frames are not server silence: {:?}",
        r.client.state()
    );
    assert!(r.client.is_sealed());
    assert_eq!(r.attempts(), 1, "fresh state needs no second attempt");
}

#[test]
fn a_slow_client_still_times_out_on_a_server_that_never_finishes_the_handshake() {
    // Every key init is lost: the server answers each connect request and nothing after it.
    let rule: Rule = Box::new(|dir, bytes| {
        let init = split_datagram(bytes).is_some_and(|(k, _)| k == PacketKind::KeyInit);
        if dir == Dir::ToServer && init {
            Vec::new()
        } else {
            vec![bytes.to_vec()]
        }
    });
    let mut r = rig(|_| {}, |_| {}, rule);
    let mut frames = 0;
    while r.ended().is_none() && frames < 200 {
        r.slow_frame(1_700);
        frames += 1;
    }
    assert_eq!(
        r.ended(),
        Some(&ClientEnd::ConnectTimeout),
        "after {frames} frames"
    );
    // At most two fresh attempts after the first, then the timeout ends it.
    assert!(r.attempts() <= 3, "attempts {}", r.attempts());
    assert_eq!(r.server.session().len(), 0);
}

#[test]
fn a_silent_server_still_times_out_at_the_configured_time_for_a_fast_client() {
    let rule: Rule = Box::new(|_, _| Vec::new());
    let mut r = rig(|_| {}, |c| c.connect_timeout_ms = 2_000, rule);
    let mut steps = 0u64;
    while r.ended().is_none() {
        r.step();
        steps += 1;
        assert!(steps <= 1_000, "never timed out");
    }
    assert_eq!(r.ended(), Some(&ClientEnd::ConnectTimeout));
    // The first update starts the clock; the timeout fires on the update 2 s after it.
    assert_eq!(steps * STEP_MS, 2_000 + STEP_MS);
    assert_eq!(r.attempts(), 1);
}

/// ADR-3075 (found by the two-machine match): a windowed client on a software renderer polled once,
/// sent its hello, then stalled for longer than the connect timeout while its first frame built
/// pipelines. The server's challenge waited in its socket; the next poll handled it and then ended
/// the attempt with `connect_timeout` in the same poll. The timeout now counts from the handshake's
/// last step, so the client answers and connects.
#[test]
fn a_client_that_stalls_after_its_hello_still_connects() {
    let mut r = rig(|_| {}, |_| {}, forward());
    r.step();
    assert!(
        !r.mid.sent(Dir::ToClient, PacketKind::Challenge).is_empty(),
        "the challenge is on its way"
    );
    // Longer than the 5 s connect timeout, with nothing polled in between.
    r.now += 6_000;
    r.connect();
    assert!(r.client.is_connected());
    assert!(r.ended().is_none());
}

/// The reset is per step, not per datagram: a key reply that never arrives still ends the
/// attempt with `connect_timeout`, within three timeouts of the start.
#[test]
fn repeated_challenges_do_not_hold_an_attempt_open() {
    let rule: Rule = Box::new(|dir, bytes| {
        let reply = split_datagram(bytes).is_some_and(|(k, _)| k == PacketKind::KeyReply);
        if dir == Dir::ToClient && reply {
            Vec::new()
        } else {
            vec![bytes.to_vec()]
        }
    });
    let mut r = rig(|_| {}, |c| c.connect_timeout_ms = 2_000, rule);
    assert_eq!(r.run_to_end(), ClientEnd::ConnectTimeout);
    assert!(r.now < 3 * 2_000, "ended at {} ms", r.now);
}

/// ADR-3075 (found by the two-machine match, run 2): the client answered the challenge, the network
/// lost the answer, and the client's first frame then stalled 6 s (pipelines on a software
/// renderer). The stall counts only `CONNECT_STALL_MS` against the connect timeout, so the client
/// resends the answer on its next poll and connects.
#[test]
fn a_client_whose_answer_was_lost_before_a_stall_still_connects() {
    let mut to_server = 0;
    let rule: Rule = Box::new(move |dir, bytes| {
        if dir == Dir::ToServer {
            to_server += 1;
            if to_server == 2 {
                return Vec::new();
            }
        }
        vec![bytes.to_vec()]
    });
    let mut r = rig(|_| {}, |_| {}, rule);
    r.steps(2);
    assert!(!r.client.is_connected(), "the answer was lost");
    r.now += 6_000;
    r.connect();
    assert!(r.client.is_connected());
    assert!(r.ended().is_none());
}

// Spec 120 § 11.4 / ADR-7946: deterministic handshake availability regressions.
#[test]
fn handshake_port_rebind_after_challenge_joins_before_connect_timeout() {
    for mode in [CryptoMode::Required, CryptoMode::Off] {
        let mut r = rig(
            |s| {
                s.crypto = mode;
                s.password = Some("port-proof".into());
            },
            |c| {
                c.crypto = mode;
                c.password = Some("port-proof".into());
            },
            forward(),
        );
        r.step(); // Challenge was addressed to port A; the client has not read it yet.
        assert_eq!(r.mid.sent(Dir::ToClient, PacketKind::Challenge).len(), 1);
        let old_addr = r.mid.socket.local_addr().expect("port A");
        r.mid.socket = r
            .net
            .bind(SocketAddr::new(old_addr.ip(), old_addr.port() + 1))
            .expect("port B");
        // The client still sees the same server address. Only its public source port changed.
        // Forward replies through A so they retain the server address the client dials.
        let mut inward = r.net.bind(old_addr).expect("inward address");
        for _ in 0..499 {
            r.now += STEP_MS;
            r.client.update(r.now);
            let mut buf = [0; 2048];
            while let Some((n, _)) = inward.recv_from(&mut buf).expect("outward") {
                r.mid
                    .socket
                    .send_to(&buf[..n], r.mid.server)
                    .expect("NAT outward");
            }
            r.server.update(r.now);
            while let Some((n, _)) = r.mid.socket.recv_from(&mut buf).expect("inward") {
                inward.send_to(&buf[..n], r.mid.client).expect("NAT inward");
            }
            if r.client.is_connected() {
                break;
            }
        }
        assert!(
            r.client.is_connected(),
            "port rebind must join before 5000 ms: {:?}",
            r.client.state()
        );
        assert!(r.now < ClientConfig::default().connect_timeout_ms);
    }
}

/// Review row 20: the NAT changes the client's source port after the `KeyReply` reached the client
/// and before its sealed join reaches the server. The server finds the half-open handshake by IP and
/// session token, opens the join under its keys, moves the row to the new port and answers there,
/// well before the connect timeout (before the fix the join fell to the live-peer rebind path, got
/// no answer, and the attempt ended in `ConnectTimeout`). A record with the right token from a
/// third port of that IP that does not verify moves nothing and gets no answer.
#[test]
fn handshake_port_rebind_after_key_reply_joins_before_connect_timeout() {
    let mut r = rig(
        |s| {
            s.crypto = CryptoMode::Required;
            s.password = Some("port-proof".into());
        },
        |c| {
            c.crypto = CryptoMode::Required;
            c.password = Some("port-proof".into());
        },
        forward(),
    );
    for _ in 0..100 {
        if !r.mid.sent(Dir::ToClient, PacketKind::KeyReply).is_empty() {
            break;
        }
        r.step();
    }
    assert_eq!(r.mid.sent(Dir::ToClient, PacketKind::KeyReply).len(), 1);
    assert!(
        r.mid.sent(Dir::ToServer, PacketKind::Sealed).is_empty(),
        "the client has not read the key reply yet, so no join left port A"
    );
    let old_addr = r.mid.socket.local_addr().expect("port A");
    let new_addr = SocketAddr::new(old_addr.ip(), old_addr.port() + 1);
    r.mid.socket = r.net.bind(new_addr).expect("port B");
    // Another port of the same public IP, for a forged record.
    let mut third = r
        .net
        .bind(SocketAddr::new(old_addr.ip(), old_addr.port() + 2))
        .expect("port C");
    // The client still dials A; replies come back through A (see the test above).
    let mut inward = r.net.bind(old_addr).expect("inward address");
    let mut forged_sent = false;
    let mut buf = [0; 2048];
    for _ in 0..499 {
        r.now += STEP_MS;
        r.client.update(r.now);
        let mut outward = Vec::new();
        while let Some((n, _)) = inward.recv_from(&mut buf).expect("outward") {
            outward.push(buf[..n].to_vec());
        }
        for datagram in outward {
            let sealed = split_datagram(&datagram).is_some_and(|(k, _)| k == PacketKind::Sealed);
            if sealed && !forged_sent {
                forged_sent = true;
                // The real join's token and counter with a broken tag, from port C, first.
                let mut forged = datagram.clone();
                *forged.last_mut().expect("tag") ^= 1;
                third.send_to(&forged, r.mid.server).expect("send from C");
                let failed = r.server.totals().auth_failed;
                r.server.update(r.now);
                assert_eq!(
                    r.server.totals().auth_failed,
                    failed + 1,
                    "the forgery failed the key"
                );
                assert_eq!(r.server.session().len(), 0);
                assert!(
                    third.recv_from(&mut buf).expect("C").is_none(),
                    "nothing answers port C"
                );
            }
            r.mid
                .socket
                .send_to(&datagram, r.mid.server)
                .expect("NAT outward");
        }
        r.server.update(r.now);
        while let Some((n, _)) = r.mid.socket.recv_from(&mut buf).expect("inward") {
            inward.send_to(&buf[..n], r.mid.client).expect("NAT inward");
        }
        if r.client.is_connected() {
            break;
        }
    }
    assert!(forged_sent, "the client sent its join");
    assert!(
        r.client.is_connected(),
        "a port change after the key reply must join before the connect timeout: {:?}",
        r.client.state()
    );
    assert!(r.now < ClientConfig::default().connect_timeout_ms);
    assert!(r.client.is_sealed());
    assert_eq!(r.server.session().len(), 1);
    let peer = r.server.session().peers().next().expect("a peer");
    assert_eq!(peer.addr, new_addr, "the peer lives on the new port");
    assert_eq!(r.server.totals().handshakes, 1, "no second key exchange");
    assert!(
        third.recv_from(&mut buf).expect("C").is_none(),
        "nothing ever answered port C"
    );
}

/// Review row 21: a captured plain-mode join answer (its cookie and password proof bind the IP,
/// not the port) replayed from another port of the same IP inside the cookie window makes no
/// second peer and gets no answer (before the fix it admitted one). The client's own address keeps
/// its resend path, which answers with the accept again.
#[test]
fn handshake_replayed_plain_join_answer_from_another_port_makes_no_second_peer() {
    fn joins(server: &mut NetServer<Duel, LoopbackSocket>) -> usize {
        std::iter::from_fn(|| server.poll_event())
            .filter(|e| matches!(e, ServerEvent::PeerJoined { .. }))
            .count()
    }
    let mut r = rig(
        |s| {
            s.crypto = CryptoMode::Off;
            s.password = Some("port-proof".into());
        },
        |c| {
            c.crypto = CryptoMode::Off;
            c.password = Some("port-proof".into());
        },
        forward(),
    );
    r.connect();
    assert!(!r.client.is_sealed());
    assert_eq!(joins(&mut r.server), 1);
    let answer = r
        .mid
        .sent(Dir::ToServer, PacketKind::ChallengeResponse)
        .into_iter()
        .next()
        .expect("the client's join answer");
    let box_addr = SocketAddr::from(BOX_ADDR);
    let mut replayer = r
        .net
        .bind(SocketAddr::new(box_addr.ip(), box_addr.port() + 1))
        .expect("another port of the same IP");
    replayer.send_to(&answer, r.mid.server).expect("replay");
    let refused = r.server.totals().handshakes_refused;
    r.server.update(r.now);
    assert_eq!(joins(&mut r.server), 0, "no second peer");
    assert_eq!(r.server.session().len(), 1);
    assert_eq!(r.server.totals().handshakes_refused, refused + 1);
    let mut buf = [0; 2048];
    assert!(
        replayer.recv_from(&mut buf).expect("io").is_none(),
        "no answer to the copy"
    );
    // From the client's own address the same answer is a lost-accept resend.
    r.mid.inject(Dir::ToServer, &answer);
    r.step();
    assert_eq!(joins(&mut r.server), 0);
    assert_eq!(r.server.session().len(), 1);
    assert!(r.client.is_connected());
}

#[test]
fn handshake_forged_challenge_then_real_challenge_joins() {
    let mut forged = false;
    let mut r = rig(
        |s| s.crypto = CryptoMode::Required,
        |c| c.crypto = CryptoMode::Required,
        Box::new(move |dir, bytes| {
            if !forged
                && dir == Dir::ToClient
                && let Some((PacketKind::Challenge, body)) = split_datagram(bytes)
            {
                forged = true;
                let mut ch = Challenge::decode(body).expect("challenge");
                ch.cookie[0] ^= 1;
                return vec![ch.to_datagram(), bytes.to_vec()];
            }
            vec![bytes.to_vec()]
        }),
    );
    r.connect();
}

#[test]
fn handshake_bad_cookie_gets_a_fresh_challenge_without_waiting_for_expiry() {
    let mut forged = false;
    let mut r = rig(
        |s| s.crypto = CryptoMode::Required,
        |c| c.crypto = CryptoMode::Required,
        Box::new(move |dir, bytes| {
            if !forged
                && dir == Dir::ToClient
                && let Some((PacketKind::Challenge, body)) = split_datagram(bytes)
            {
                forged = true;
                let mut ch = Challenge::decode(body).expect("challenge");
                ch.cookie[0] ^= 1;
                return vec![ch.to_datagram()]; // The original challenge was lost.
            }
            vec![bytes.to_vec()]
        }),
    );
    r.connect();
    assert!(r.mid.sent(Dir::ToClient, PacketKind::Challenge).len() >= 2);
}

#[test]
fn handshake_duplicate_challenge_retains_share_for_lost_key_reply() {
    let original = Rc::new(RefCell::new(None::<Vec<u8>>));
    let saved = original.clone();
    let mut dropped = false;
    let mut r = rig(
        |s| s.crypto = CryptoMode::Required,
        |c| c.crypto = CryptoMode::Required,
        Box::new(move |dir, bytes| {
            if dir == Dir::ToClient {
                match split_datagram(bytes).map(|(kind, _)| kind) {
                    Some(PacketKind::Challenge) => *saved.borrow_mut() = Some(bytes.to_vec()),
                    Some(PacketKind::KeyReply) if !dropped => {
                        dropped = true;
                        return vec![saved.borrow().clone().expect("original challenge")];
                    }
                    _ => {}
                }
            }
            vec![bytes.to_vec()]
        }),
    );
    r.connect();
    assert_eq!(r.server.totals().handshakes, 1);
    assert_eq!(r.server.totals().handshakes_refused, 0);
    let inits = r.mid.sent(Dir::ToServer, PacketKind::KeyInit);
    assert!(inits.len() >= 2);
    assert!(inits.iter().all(|bytes| bytes == &inits[0]));
}

#[test]
fn handshake_changed_challenges_cannot_extend_connect_timeout() {
    let mut r = rig(
        |s| s.crypto = CryptoMode::Required,
        |c| c.crypto = CryptoMode::Required,
        Box::new(|dir, bytes| {
            if dir == Dir::ToClient && split_datagram(bytes).is_some_and(|(k, _)| k == PacketKind::KeyReply) {
                Vec::new()
            } else {
                vec![bytes.to_vec()]
            }
        }),
    );
    r.step();
    let original = r
        .mid
        .sent(Dir::ToClient, PacketKind::Challenge)
        .pop()
        .expect("challenge");
    let mut challenge = Challenge::decode(split_datagram(&original).expect("wire").1).expect("decode");
    for step in 0..600_u64 {
        challenge.cookie[..8].copy_from_slice(&step.to_le_bytes());
        r.mid.inject(Dir::ToClient, &challenge.to_datagram());
        r.step();
        if r.ended().is_some() {
            break;
        }
    }
    assert_eq!(r.ended(), Some(&ClientEnd::ConnectTimeout));
    assert!(r.now <= 2 * STEP_MS + ClientConfig::default().connect_timeout_ms);
}

#[test]
fn handshake_refresh_is_bounded_and_does_no_curve_work() {
    let net = LoopbackNet::new();
    let addr = SocketAddr::from(SERVER_ADDR);
    let mut server = NetServer::<Duel, _>::with_secret(
        net.bind(addr).expect("server"),
        ServerConfig {
            pre_proof_answers_per_s: 3,
            ..ServerConfig::default()
        },
        [7; 32],
    );
    let mut raw = net.bind(SocketAddr::from(CLIENT_ADDR)).expect("client");
    let init = KeyInit {
        client_nonce: 17,
        cookie: [0; 16],
        version: CRYPTO_VERSION,
        noise: [8; 32],
    }
    .to_datagram();
    for _ in 0..20 {
        raw.send_to(&init, addr).expect("send");
    }
    server.update(0);
    let mut replies = 0;
    let mut buf = [0; 2048];
    while let Some((n, _)) = raw.recv_from(&mut buf).expect("receive") {
        assert!(n <= init.len(), "refresh cannot amplify");
        assert_eq!(
            split_datagram(&buf[..n]).map(|(kind, _)| kind),
            Some(PacketKind::Challenge)
        );
        replies += 1;
    }
    assert_eq!(replies, 3);
    assert_eq!(server.totals().pre_proof_shed, 17);
    assert_eq!(server.totals().handshakes, 0);
    assert_eq!(server.session().len(), 0);
}

#[test]
fn handshake_plaintext_challenge_after_key_reply_cannot_replace_keys() {
    let mut saved = None;
    let mut r = rig(
        |s| s.crypto = CryptoMode::Required,
        |c| c.crypto = CryptoMode::Required,
        Box::new(move |dir, bytes| {
            if dir == Dir::ToClient {
                match split_datagram(bytes) {
                    Some((PacketKind::Challenge, body)) => {
                        saved = Some(Challenge::decode(body).expect("challenge"))
                    }
                    Some((PacketKind::KeyReply, _)) => {
                        let mut forged = saved.expect("challenge before reply");
                        forged.cookie[0] ^= 1;
                        return vec![bytes.to_vec(), forged.to_datagram()];
                    }
                    _ => {}
                }
            }
            vec![bytes.to_vec()]
        }),
    );
    r.connect();
    assert!(r.client.is_sealed());
    assert_eq!(r.server.totals().handshakes, 1);
}

#[test]
fn handshake_challenge_race_after_key_init_recovers_without_extra_curve_work_on_the_cookie() {
    let mut original = None;
    let mut forged = false;
    let mut r = rig(
        |s| s.crypto = CryptoMode::Required,
        |c| c.crypto = CryptoMode::Required,
        Box::new(move |dir, bytes| {
            if dir == Dir::ToClient {
                match split_datagram(bytes) {
                    Some((PacketKind::Challenge, body)) => {
                        original = Some(Challenge::decode(body).expect("challenge"))
                    }
                    Some((PacketKind::KeyReply, _)) if !forged => {
                        forged = true;
                        let mut ch = original.expect("challenge before key reply");
                        ch.cookie[0] ^= 1;
                        return vec![ch.to_datagram()]; // Reply is lost; its pending server row remains.
                    }
                    _ => {}
                }
            }
            vec![bytes.to_vec()]
        }),
    );
    r.connect();
    assert!(r.now < ClientConfig::default().connect_timeout_ms);
    // Review row 18 (ADR-7995): the client keeps the exchange whose KeyInit the server stored,
    // so the retransmitted KeyReply finishes it. Before, the forged challenge cost that share
    // and the join needed a second nonce and a second curve exchange on the server.
    assert_eq!(
        r.server.totals().handshakes,
        1,
        "the stored share finishes the join: no second exchange"
    );
    let mut nonces = std::collections::BTreeSet::new();
    for request in r.mid.sent(Dir::ToServer, PacketKind::ConnectRequest) {
        nonces.insert(
            ConnectRequest::decode(split_datagram(&request).expect("frame").1)
                .expect("request")
                .client_nonce,
        );
    }
    assert_eq!(nonces.len(), 1, "no conflicting share, so no new attempt");
}

#[test]
fn handshake_repeated_pending_share_races_exhaust_restarts_without_extending_the_deadline() {
    let mut original = None;
    let mut r = rig(
        |s| s.crypto = CryptoMode::Required,
        |c| c.crypto = CryptoMode::Required,
        Box::new(move |dir, bytes| {
            if dir == Dir::ToClient {
                match split_datagram(bytes) {
                    Some((PacketKind::Challenge, body)) => {
                        original = Some(Challenge::decode(body).expect("challenge"))
                    }
                    Some((PacketKind::KeyReply, _)) => {
                        let mut forged = original.expect("challenge before key reply");
                        forged.cookie[0] ^= 1;
                        return vec![forged.to_datagram()];
                    }
                    _ => {}
                }
            }
            vec![bytes.to_vec()]
        }),
    );
    for _ in 0..600 {
        r.step();
        if r.ended().is_some() {
            break;
        }
    }
    assert_eq!(r.ended(), Some(&ClientEnd::ConnectTimeout));
    assert_eq!(r.attempts(), 3, "the existing two-restart cap still applies");
    assert_eq!(r.server.totals().handshakes, 3, "one exchange per nonce");
    assert!(r.now <= 2 * STEP_MS + ClientConfig::default().connect_timeout_ms);
}
