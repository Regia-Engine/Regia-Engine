// SPDX-License-Identifier: MPL-2.0
//! Mutation fuzzing of everything an attacker can reach before and after the key exchange
//! (ADR-1901): the handshake and record parsers, the `server.key` reader, and live endpoints fed
//! mutated traffic. Every run is seeded and bounded, so a failure reproduces.
//!
//! `regia_test_utils::fuzz` is the shared machinery, but it depends on the whole engine, which
//! would make a layer-2 crate's tests build the renderer; this file keeps a small copy of the
//! same idea (SplitMix64, byte mutators).

mod common;

use std::net::SocketAddr;

use common::{Duel, DuelMsg, Team};
use regia_net::crypto::{
    ClientHandshake, CryptoError, KeyInit, KeyReply, SealedHeader, SealedKind, SecureLink, ServerIdentity,
    ServerKey, accept_handshake,
};
use regia_net::handshake::{
    Accept, Challenge, ChallengeResponse, ConnectRequest, Deny, PacketKind, split_datagram,
};
use regia_net::prelude::*;
use regia_net::{LoopbackSocket, NetSettings};

const ITERATIONS: usize = 4_000;
/// Each of these runs curve multiplications, so the count stays lower.
const NOISE_ITERATIONS: usize = 512;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One random corruption: bit flips, overwrites, truncation, extension, splice or a hostile word.
fn mutate(input: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..1 + rng.below(4) {
        match rng.below(6) {
            0 if !out.is_empty() => {
                let i = rng.below(out.len());
                out[i] ^= 1 << rng.below(8);
            }
            1 if !out.is_empty() => {
                let i = rng.below(out.len());
                out[i] = rng.next() as u8;
            }
            2 => out.truncate(rng.below(out.len() + 1)),
            3 => {
                let extra = rng.below(32);
                out.extend((0..extra).map(|_| 0xA5));
            }
            4 if out.len() >= 8 => {
                let at = rng.below(out.len() - 7);
                let word = [0u64, 1, u64::MAX, u64::MAX - 1, 1 << 63][rng.below(5)];
                out[at..at + 8].copy_from_slice(&word.to_le_bytes());
            }
            5 if out.len() >= 2 => {
                let (a, b) = (rng.below(out.len()), rng.below(out.len()));
                out.swap(a, b);
            }
            _ => {}
        }
    }
    out
}

/// Runs every parser an attacker can reach on `bytes`. Each must return, never panic.
fn parse_all(bytes: &[u8]) {
    let body = split_datagram(bytes).map_or(bytes, |(_, b)| b);
    let _ = ConnectRequest::decode(body);
    let _ = Challenge::decode(body);
    let _ = ChallengeResponse::decode(body);
    let _ = Accept::decode(body);
    let _ = Deny::decode(body);
    let _ = KeyInit::decode(body);
    let _ = KeyReply::decode(body);
    let _ = SealedHeader::parse(body);
}

#[test]
fn handshake_and_record_parsers_never_panic() {
    let init = KeyInit {
        client_nonce: 1,
        cookie: [2; 16],
        version: 1,
        noise: [3; 32],
    };
    let reply = KeyReply {
        client_nonce: 1,
        noise: [4; 100],
    };
    let request = ConnectRequest {
        protocol_id: 1,
        client_nonce: 2,
        crypto_version: 1,
        crypto_required: true,
    };
    let challenge = Challenge {
        client_nonce: 1,
        cookie: [1; 16],
        password: None,
        crypto_version: 1,
    };
    let fixtures = [
        init.to_datagram(),
        reply.to_datagram(),
        request.to_datagram(),
        challenge.to_datagram(),
    ];
    let mut rng = Rng(0xC0);
    for fixture in &fixtures {
        parse_all(fixture);
        for n in 0..fixture.len() {
            parse_all(&fixture[..n]);
        }
        for _ in 0..ITERATIONS {
            parse_all(&mutate(fixture, &mut rng));
        }
    }
}

#[test]
fn key_text_readers_refuse_garbage_without_panicking() {
    let identity = ServerIdentity::generate().expect("random");
    let text = format!(
        "(format_version: 1, private_key: \"{}\", public_key: \"{}\")",
        "0".repeat(64),
        identity.public()
    );
    let mut rng = Rng(0xC1);
    let mut refused = 0usize;
    for _ in 0..ITERATIONS {
        let bytes = mutate(text.as_bytes(), &mut rng);
        let text = String::from_utf8_lossy(&bytes);
        refused += usize::from(ServerIdentity::from_file_text(&text).is_err());
        let _ = ServerKey::parse(&text);
    }
    assert!(refused > ITERATIONS / 2, "garbage is refused: {refused}");
}

#[test]
fn net_settings_refuse_garbage_without_panicking() {
    let text = "(session: (encryption: Auto))";
    assert_eq!(
        NetSettings::from_ron(text).expect("valid").session.encryption,
        regia_net::CryptoMode::Auto
    );
    let mut rng = Rng(0xC2);
    for _ in 0..ITERATIONS {
        let bytes = mutate(text.as_bytes(), &mut rng);
        let _ = NetSettings::from_ron(&String::from_utf8_lossy(&bytes));
    }
}

/// A finished handshake, for tests that go straight at the record layer.
fn links() -> (SecureLink, SecureLink) {
    let identity = ServerIdentity::generate().expect("random");
    let mut hs = ClientHandshake::start(&[7; 32], Some(identity.public())).expect("start");
    let (server, reply) = accept_handshake(&identity, &[7; 32], hs.message(), 0x5EA1).expect("accept");
    hs.read_reply(&reply).expect("reply");
    (hs.into_link().expect("link"), server)
}

/// The sealed body of a datagram, whatever its first bytes say: the router's kind check is a
/// separate step, so these tests hand every mutated body to the record layer.
fn sealed_body(datagram: &[u8]) -> &[u8] {
    split_datagram(datagram).map_or(datagram, |(_, body)| body)
}

#[test]
fn the_record_layer_opens_no_mutated_record_and_forgeries_leave_it_usable() {
    let (mut client, mut server) = links();
    let records: Vec<Vec<u8>> = [0usize, 1, 64, 1000]
        .iter()
        .cycle()
        .take(32)
        .enumerate()
        .map(|(i, &len)| {
            client
                .seal(SealedKind::Payload, &vec![i as u8; len])
                .expect("seal")
        })
        .collect();
    let mut rng = Rng(0xC5);
    let mut refused_at_the_tag = 0usize;
    for _ in 0..ITERATIONS {
        let record = &records[rng.below(records.len())];
        let bytes = mutate(record, &mut rng);
        let body = sealed_body(&bytes);
        if body == sealed_body(record) {
            continue; // Only the outer kind changed: that body is genuine and the router refuses it.
        }
        let Some(header) = SealedHeader::parse(body) else {
            continue;
        };
        match server.open(header) {
            Err(CryptoError::Auth) => refused_at_the_tag += 1,
            Err(_) => {}
            Ok(opened) => panic!("a mutated record opened: {opened:?}"),
        }
    }
    assert!(
        refused_at_the_tag > ITERATIONS / 2,
        "mutations must reach the tag check: {refused_at_the_tag}"
    );
    // Forged counters moved nothing: every genuine record still opens, once.
    for (i, record) in records.iter().enumerate() {
        let header = SealedHeader::parse(sealed_body(record)).expect("header");
        let (kind, body) = server.open(header).expect("genuine");
        assert_eq!(
            (kind, body.first().copied()),
            (SealedKind::Payload, (!body.is_empty()).then_some(i as u8))
        );
        let header = SealedHeader::parse(sealed_body(record)).expect("header");
        assert_eq!(server.open(header), Err(CryptoError::Replay));
    }
}

#[test]
fn a_mutated_key_reply_never_finishes_the_handshake_and_never_ends_it() {
    let identity = ServerIdentity::generate().expect("random");
    let mut hs = ClientHandshake::start(&[8; 32], Some(identity.public())).expect("start");
    let (mut server, reply) = accept_handshake(&identity, &[8; 32], hs.message(), 9).expect("accept");
    let mut rng = Rng(0xC6);
    for _ in 0..NOISE_ITERATIONS {
        let bytes = mutate(&reply, &mut rng);
        if bytes != reply {
            assert_eq!(hs.read_reply(&bytes), Err(CryptoError::Auth));
        }
    }
    assert_eq!(hs.read_reply(&reply), Ok(identity.public()));
    let mut client = hs.into_link().expect("link");
    let record = client.seal(SealedKind::Payload, b"after").expect("seal");
    let header = SealedHeader::parse(sealed_body(&record)).expect("header");
    assert_eq!(server.open(header), Ok((SealedKind::Payload, b"after".to_vec())));
}

#[test]
fn a_mutated_first_message_gives_no_session_with_the_real_client() {
    let identity = ServerIdentity::generate().expect("random");
    let hs = ClientHandshake::start(&[9; 32], Some(identity.public())).expect("start");
    let genuine = *hs.message();
    let mut hs = hs;
    let mut rng = Rng(0xC7);
    let mut answered = 0usize;
    for _ in 0..NOISE_ITERATIONS {
        let bytes = mutate(&genuine, &mut rng);
        if bytes == genuine {
            continue;
        }
        // Any 32 bytes are a Curve25519 share, so the server may answer; the answer is useless.
        if let Ok((_, reply)) = accept_handshake(&identity, &[9; 32], &bytes, 9) {
            answered += 1;
            assert_eq!(hs.read_reply(&reply), Err(CryptoError::Auth));
        }
    }
    assert!(answered > 0, "the mutator keeps some messages at 32 bytes");
    let (_, reply) = accept_handshake(&identity, &[9; 32], &genuine, 9).expect("accept");
    assert_eq!(hs.read_reply(&reply), Ok(identity.public()));
}

struct Pair {
    server: NetServer<Duel, LoopbackSocket>,
    client: NetClient<Duel, LoopbackSocket>,
    attacker_server_side: LoopbackSocket,
    server_addr: SocketAddr,
    client_addr: SocketAddr,
    tap: LoopbackSocket,
    captured: Vec<Vec<u8>>,
    now: u64,
}

/// A sealed pair; `tap` relays between them and keeps every datagram for mutation.
fn pair() -> Pair {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(([10, 0, 0, 1], 7777));
    let tap_addr = SocketAddr::from(([10, 0, 0, 9], 7000));
    let client_addr = SocketAddr::from(([10, 0, 0, 2], 5000));
    let attacker = SocketAddr::from(([10, 0, 0, 66], 6666));
    let server = NetServer::with_secret(
        net.bind(server_addr).expect("bind"),
        ServerConfig {
            password: Some("pw".into()),
            ..ServerConfig::default()
        },
        [9; 32],
    );
    let client = NetClient::connect(
        net.bind(client_addr).expect("bind"),
        tap_addr,
        ClientConfig {
            password: Some("pw".into()),
            ..ClientConfig::default()
        },
        &Team(1),
    )
    .expect("fits");
    Pair {
        server,
        client,
        attacker_server_side: net.bind(attacker).expect("bind"),
        server_addr,
        client_addr,
        tap: net.bind(tap_addr).expect("bind"),
        captured: Vec::new(),
        now: 0,
    }
}

impl Pair {
    fn pump(&mut self) {
        let mut buf = [0u8; 2048];
        while let Some((n, from)) = self.tap.recv_from(&mut buf).expect("io") {
            self.captured.push(buf[..n].to_vec());
            let to = if from == self.server_addr {
                self.client_addr
            } else {
                self.server_addr
            };
            self.tap.send_to(&buf[..n], to).expect("send");
        }
    }

    fn step(&mut self) {
        self.now += 10;
        self.client.update(self.now);
        self.pump();
        self.server.update(self.now);
        self.pump();
    }
}

#[test]
fn live_endpoints_survive_mutated_traffic_and_deliver_no_forgery() {
    let mut p = pair();
    for _ in 0..200 {
        if p.client.is_connected() {
            break;
        }
        p.step();
    }
    assert!(
        p.client.is_connected() && p.client.is_sealed(),
        "{:?}",
        p.client.state()
    );
    while p.server.poll_event().is_some() {}
    while p.client.poll_event().is_some() {}
    p.client
        .send(Channel::GAME_RPC, &DuelMsg::Chat("genuine".into()))
        .expect("queued");
    for _ in 0..30 {
        p.step();
    }
    let fixtures = p.captured.clone();
    assert!(
        fixtures
            .iter()
            .any(|d| split_datagram(d).is_some_and(|(k, _)| k == PacketKind::Sealed)),
        "the corpus holds sealed records"
    );
    let chats = |events: Vec<ServerEvent<Duel>>| {
        events
            .into_iter()
            .filter(|e| matches!(e, ServerEvent::Message { .. }))
            .count()
    };
    assert_eq!(chats(std::iter::from_fn(|| p.server.poll_event()).collect()), 1);

    let mut rng = Rng(0xC3);
    for round in 0..ITERATIONS {
        let fixture = &fixtures[rng.below(fixtures.len())];
        let bytes = mutate(fixture, &mut rng);
        // Through the established flow on both sides, and from a stranger's address.
        p.tap.send_to(&bytes, p.server_addr).expect("send");
        p.tap.send_to(&bytes, p.client_addr).expect("send");
        p.attacker_server_side
            .send_to(&bytes, p.server_addr)
            .expect("send");
        if round.is_multiple_of(16) {
            p.step();
        }
    }
    p.step();
    let events: Vec<ServerEvent<Duel>> = std::iter::from_fn(|| p.server.poll_event()).collect();
    let forged = events
        .iter()
        .filter(|e| matches!(e, ServerEvent::Message { .. }))
        .count();
    assert_eq!(forged, 0, "no mutation may decode as a message");
    assert!(p.client.is_connected(), "{:?}", p.client.state());
    assert_eq!(p.server.session().len(), 1, "the session survived");
    assert!(p.server.totals().auth_failed > 0);
    p.client
        .send(Channel::GAME_RPC, &DuelMsg::Chat("after".into()))
        .expect("queued");
    for _ in 0..30 {
        p.step();
    }
    assert_eq!(chats(std::iter::from_fn(|| p.server.poll_event()).collect()), 1);
}

#[test]
fn endpoints_in_every_handshake_stage_survive_pure_noise() {
    // Noise at each step of a join: the join still finishes, because the server keeps resending.
    let mut p = pair();
    let mut rng = Rng(0xC4);
    for _ in 0..400 {
        for _ in 0..4 {
            let len = rng.below(300);
            let mut bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if bytes.len() >= 5 {
                bytes[..4].copy_from_slice(&regia_net::handshake::MAGIC);
                bytes[4] = 1 + rng.below(10) as u8;
            }
            p.tap.send_to(&bytes, p.client_addr).expect("send");
            p.tap.send_to(&bytes, p.server_addr).expect("send");
        }
        p.step();
        if p.client.is_connected() {
            break;
        }
    }
    assert!(
        p.client.is_connected() && p.client.is_sealed(),
        "{:?}",
        p.client.state()
    );
}
