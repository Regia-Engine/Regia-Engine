// SPDX-License-Identifier: MPL-2.0
//! Hostile datagrams past the decode (ADR-1917): a server and two clients run over the in-memory loopback while
//! a wrapper socket corrupts a seeded share of one client's real traffic in flight (so the corrupt packets carry
//! that client's address and session token and reach the connected-packet path), and an attacker socket sprays
//! mutated handshake datagrams and connected headers with guessed tokens. Nothing may panic or stall; the clean
//! client must stay connected and keep exchanging messages. Time is a step counter; nothing sleeps.

mod common;

use std::io;
use std::net::SocketAddr;

use common::{Duel, DuelMsg, Team};
use regia_core::fuzz::{FuzzRng, mutate_bytes};
use regia_net::handshake::{self, ConnectRequest, PacketKind};
use regia_net::prelude::*;
use regia_net::{DatagramSocket, LoopbackSocket};

const STEP_MS: u64 = 10;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Corrupts a seeded share of outgoing datagrams (mutated in place of the original).
struct Corrupting {
    inner: LoopbackSocket,
    rng: FuzzRng,
    one_in: usize,
    corrupted: u64,
}

impl DatagramSocket for Corrupting {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if self.rng.chance(self.one_in) {
            self.corrupted += 1;
            let bad = mutate_bytes(bytes, &mut self.rng);
            self.inner.send_to(&bad, dest)
        } else {
            self.inner.send_to(bytes, dest)
        }
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.inner.recv_from(buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

fn config(name: &str) -> ClientConfig {
    ClientConfig {
        name: name.into(),
        ..ClientConfig::default()
    }
}

/// A seeded spray of hostile datagrams: mutated connect requests, connected headers with guessed tokens and junk.
fn hostile(rng: &mut FuzzRng) -> Vec<u8> {
    match rng.below(3) {
        0 => {
            let request = ConnectRequest {
                protocol_id: rng.next_u64(),
                client_nonce: rng.next_u64(),
                crypto_version: rng.byte(),
                crypto_required: rng.below(2) == 0,
            }
            .to_datagram();
            mutate_bytes(&request, rng)
        }
        1 => {
            let kind = [PacketKind::Payload, PacketKind::Disconnect][rng.below(2)];
            let mut d = handshake::connected_header(kind, rng.next_u64() as u32).to_vec();
            d.extend((0..rng.below(64)).map(|_| rng.byte()));
            d
        }
        _ => (0..rng.below(300)).map(|_| rng.byte()).collect(),
    }
}

#[test]
fn server_and_clients_survive_corrupted_traffic() {
    for seed in [1u64, 2, 3] {
        let net = LoopbackNet::new();
        let mut server: NetServer<Duel, LoopbackSocket> = NetServer::with_secret(
            net.bind(addr(7777)).expect("bind server"),
            ServerConfig::default(),
            [7; 32],
        );
        let mut clean: NetClient<Duel, LoopbackSocket> = NetClient::connect(
            net.bind(addr(0)).expect("bind"),
            addr(7777),
            config("clean"),
            &Team(0),
        )
        .expect("join fits");
        let noisy_socket = Corrupting {
            inner: net.bind(addr(0)).expect("bind"),
            rng: FuzzRng::new(seed),
            one_in: 4,
            corrupted: 0,
        };
        let mut noisy: NetClient<Duel, Corrupting> =
            NetClient::connect(noisy_socket, addr(7777), config("noisy"), &Team(1)).expect("join fits");
        let mut attacker = net.bind(addr(9000)).expect("bind attacker");
        let mut rng = FuzzRng::new(seed ^ 0xA77A);

        let mut now = 0;
        let mut chats_received = 0usize;
        for step in 0..600u64 {
            now += STEP_MS;
            for _ in 0..4 {
                let _ = attacker.send_to(&hostile(&mut rng), addr(7777));
            }
            if step % 10 == 0 {
                let _ = clean.send(Channel::GAME_RPC, &DuelMsg::Chat(format!("c{step}")));
                let _ = noisy.send(Channel::Unreliable, &DuelMsg::Chat(format!("n{step}")));
            }
            clean.update(now);
            noisy.update(now);
            server.update(now);
            while let Some(event) = server.poll_event() {
                if let ServerEvent::Message {
                    message: DuelMsg::Chat(text),
                    ..
                } = event
                    && text.starts_with('c')
                {
                    chats_received += 1;
                }
            }
            while clean.poll_event().is_some() {}
            while noisy.poll_event().is_some() {}
            // The attacker's socket drains whatever the server answers.
            let mut buf = [0u8; 2048];
            while let Ok(Some(_)) = attacker.recv_from(&mut buf) {}
        }
        assert!(clean.is_connected(), "seed {seed}: the clean client was dropped");
        assert!(
            chats_received >= 50,
            "seed {seed}: only {chats_received} of 60 reliable chats from the clean client arrived"
        );
    }
}

/// A tick sample with a capped command tail, the shape a game's input codec takes (ADR-7892).
#[derive(Debug, Clone, PartialEq)]
enum Sample {
    /// A sample whose tail fits.
    Ok(Vec<u8>),
    /// Encodes a tail past [`TAIL_CAP`].
    OverCap(u16),
    /// Encodes seeded junk: an unknown tag, a short tail or stray bytes.
    Junk(Vec<u8>),
}

const TAIL_CAP: usize = 24;

impl NetCodec for Sample {
    fn encode(&self, w: &mut WireWriter) {
        match self {
            Sample::Ok(tail) => {
                w.u8(1);
                w.u16(tail.len() as u16);
                w.raw(tail);
            }
            Sample::OverCap(n) => {
                w.u8(1);
                w.u16(*n);
                w.raw(&vec![0x5A; usize::from(*n)]);
            }
            Sample::Junk(bytes) => w.raw(bytes),
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            1 => Ok(Sample::Ok(r.bytes_u16(TAIL_CAP)?.to_vec())),
            _ => Err(WireError::Invalid("sample tag")),
        }
    }
}

#[derive(Debug)]
struct Tails;

impl NetGame for Tails {
    type Message = u32;
    type Input = Sample;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.fuzz_tails";
    const PROTOCOL_VERSION: u32 = 1;
}

/// Seeded junk for a sample: never a valid tag-1 sample with a tail that fits and nothing after it.
fn junk_sample(rng: &mut FuzzRng) -> Vec<u8> {
    match rng.below(3) {
        // An unknown tag and random bytes.
        0 => {
            let mut b = vec![2 + rng.byte() % 250];
            b.extend((0..rng.below(40)).map(|_| rng.byte()));
            b
        }
        // A tail shorter than its length prefix says (truncated).
        1 => {
            let len = 8 + rng.below(TAIL_CAP - 8);
            let mut b = vec![1, len as u8, 0];
            b.extend((0..rng.below(len)).map(|_| rng.byte()));
            b
        }
        // A valid sample, then stray bytes.
        _ => {
            let mut b = vec![1, 0, 0];
            b.extend((0..1 + rng.below(8)).map(|_| rng.byte()));
            b
        }
    }
}

/// Seeded hostile input streams (ADR-7892): several players send a seeded mix of valid samples and
/// undecodable ones, then one sample whose command tail passes the codec's cap at a seeded tick. Every
/// undecodable sample is refused (`ServerEvent::Rejected`), the over-cap one disconnects exactly its
/// sender with `ProtocolError` at once, and the clean player never misses a tick.
#[test]
fn over_cap_input_tails_drop_only_their_senders() {
    const HOSTILE: usize = 4;
    for seed in [11u64, 12, 13] {
        let mut rng = FuzzRng::new(seed);
        let net = LoopbackNet::new();
        let mut server: NetServer<Tails, LoopbackSocket> = NetServer::with_secret(
            net.bind(addr(7777)).expect("bind server"),
            ServerConfig::default(),
            [9; 32],
        );
        let connect = |name: &str, redundancy: usize| {
            NetClient::<Tails, LoopbackSocket>::connect(
                net.bind(addr(0)).expect("bind"),
                addr(7777),
                ClientConfig {
                    name: name.into(),
                    input_redundancy: redundancy,
                    ..ClientConfig::default()
                },
                &(),
            )
            .expect("join fits")
        };
        let mut clean = connect("clean", 4);
        let mut hostile: Vec<_> = (0..HOSTILE).map(|i| connect(&format!("h{i}"), 1)).collect();
        let mut now = 0;
        for _ in 0..200 {
            if clean.is_connected() && hostile.iter().all(|c| c.is_connected()) {
                break;
            }
            now += STEP_MS;
            clean.update(now);
            hostile.iter_mut().for_each(|c| c.update(now));
            server.update(now);
        }
        assert!(clean.is_connected() && hostile.iter().all(|c| c.is_connected()));
        let clean_peer = clean.peer_id().expect("clean id");
        let hostile_peers: Vec<PeerId> = hostile.iter().map(|c| c.peer_id().expect("id")).collect();
        while server.poll_event().is_some() {}
        let over_cap_at: Vec<usize> = (0..HOSTILE).map(|_| 5 + rng.below(80)).collect();
        let mut junk_sent = [0usize; HOSTILE];
        let mut rejected = [0usize; HOSTILE];
        let mut left = [None; HOSTILE];
        let mut fatal = [0usize; HOSTILE];
        for step in 0..100usize {
            now += STEP_MS;
            let tick = server.driver().tick();
            clean
                .send_input(tick, Sample::Ok(vec![step as u8; 1 + step % TAIL_CAP]))
                .expect("clean input");
            for (i, c) in hostile.iter_mut().enumerate() {
                // Past its over-cap tick the peer is gone; it may not have heard yet.
                if step > over_cap_at[i] {
                    continue;
                }
                let sample = if step == over_cap_at[i] {
                    Sample::OverCap((TAIL_CAP + 1 + rng.below(200)) as u16)
                } else if rng.chance(2) {
                    junk_sent[i] += 1;
                    Sample::Junk(junk_sample(&mut rng))
                } else {
                    Sample::Ok(vec![rng.byte(); rng.below(TAIL_CAP + 1)])
                };
                c.send_input(tick, sample).expect("hostile input");
            }
            clean.update(now);
            hostile.iter_mut().for_each(|c| c.update(now));
            server.receive(now);
            while let Some(event) = server.poll_event() {
                match event {
                    ServerEvent::Rejected { peer, kind, .. } => {
                        let i = hostile_peers
                            .iter()
                            .position(|p| *p == peer)
                            .unwrap_or_else(|| panic!("seed {seed}: the clean peer was rejected ({kind:?})"));
                        assert!(left[i].is_none(), "seed {seed}: a rejection after the peer left");
                        if kind == RejectKind::InputTooLong {
                            fatal[i] += 1;
                            assert_eq!(
                                step, over_cap_at[i],
                                "seed {seed}: fatal only at the over-cap tick"
                            );
                        } else {
                            assert_eq!(kind, RejectKind::InputMalformed, "seed {seed}");
                            rejected[i] += 1;
                        }
                    }
                    ServerEvent::PeerLeft { peer, reason } => {
                        let i = hostile_peers
                            .iter()
                            .position(|p| *p == peer)
                            .unwrap_or_else(|| panic!("seed {seed}: the clean peer left ({reason:?})"));
                        assert_eq!(step, over_cap_at[i], "seed {seed}: h{i} left at once");
                        left[i] = Some(reason);
                    }
                    _ => {}
                }
            }
            let inputs = server.advance_tick(|_, _| {});
            assert!(
                inputs.contains_key(&clean_peer),
                "seed {seed}: the clean player missed tick {tick}"
            );
            server.flush(now);
        }
        for (i, c) in hostile.iter().enumerate() {
            assert_eq!(
                left[i],
                Some(DisconnectReason::ProtocolError),
                "seed {seed}: h{i}"
            );
            assert_eq!(fatal[i], 1, "seed {seed}: h{i}");
            assert_eq!(
                rejected[i], junk_sent[i],
                "seed {seed}: h{i}: every junk sample refused"
            );
            assert!(!c.is_connected(), "seed {seed}: h{i} heard the disconnect");
        }
        assert!(clean.is_connected(), "seed {seed}");
        assert_eq!(server.driver().missed(clean_peer), 0, "seed {seed}");
    }
}
