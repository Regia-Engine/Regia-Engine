// SPDX-License-Identifier: MPL-2.0
//! Client input larger than the redundancy budget, over real UDP on 127.0.0.1 with the link
//! sealed as shipped (`CryptoMode::Required`, ADR-7896, DO27 request NET_N1B).
//!
//! The DO27 shape: a structurally valid order naming 128 units is a 548-byte sample; at
//! redundancy 15 behind a 1200-byte packet cap the whole history no longer fits one datagram.
//! Before ADR-7896 the third such sample failed with `TooLarge { len: 1654, max: 1154 }`, and
//! because the refused sample had already entered the history every later send failed too.
//!
//! Time is a step counter (10 ms per step) and every wait is a bounded step loop on a condition;
//! nothing sleeps and no assertion reads the wall clock.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use regia_net::handshake::{ConnectRequest, Deny, PacketKind, split_datagram};
use regia_net::prelude::*;
use regia_net::wire::fnv1a_64;
use regia_net::{CryptoMode, LinkConfig, MAX_RELIABLE_INPUT_BYTES, SendError};

const STEP_MS: u64 = 10;
/// The packet cap DO27 runs (the `LinkConfig` default).
const PACKET_CAP: usize = 1200;
/// DO27's 128-unit order: 546 payload bytes, a 548-byte `Vec<u8>` sample.
const ORDER_PAYLOAD: usize = 546;

/// The test game: inputs are opaque byte samples (`len u16 | bytes`).
struct Orders;

impl NetGame for Orders {
    type Message = Vec<u8>;
    type Input = Vec<u8>;
    type JoinData = ();
    const GAME_ID: &'static str = "test.input-mtu";
    const PROTOCOL_VERSION: u32 = 1;
}

/// The same game id and version as [`Orders`], so it shares the protocol id, but its input
/// encoder writes raw bytes with no length prefix: a hostile build of the same game.
struct HostileOrders;

#[derive(Debug, Clone, PartialEq)]
struct RawInput(Vec<u8>);

impl NetCodec for RawInput {
    fn encode(&self, w: &mut WireWriter) {
        w.raw(&self.0);
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self(r.take(r.remaining())?.to_vec()))
    }
}

impl NetGame for HostileOrders {
    type Message = Vec<u8>;
    type Input = RawInput;
    type JoinData = ();
    const GAME_ID: &'static str = "test.input-mtu";
    const PROTOCOL_VERSION: u32 = 1;
}

/// A real UDP socket that records the largest datagram it sent and can black out its sends.
struct Watched {
    inner: UdpTransport,
    largest_out: Rc<Cell<usize>>,
    blackout: Rc<Cell<bool>>,
}

impl DatagramSocket for Watched {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.largest_out.set(self.largest_out.get().max(bytes.len()));
        if self.blackout.get() {
            return Ok(()); // Lost on the way.
        }
        self.inner.send_to(bytes, dest)
    }
    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.inner.recv_from(buf)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

fn localhost() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

struct Rig<G: NetGame> {
    server: NetServer<Orders, UdpTransport>,
    client: NetClient<G, Watched>,
    largest_out: Rc<Cell<usize>>,
    blackout: Rc<Cell<bool>>,
    now: u64,
    /// Inputs the server simulated, by tick.
    simulated: BTreeMap<u64, Vec<u8>>,
    /// Ticks the server simulated without this peer's input.
    gaps: Vec<u64>,
}

impl<G: NetGame<JoinData = ()>> Rig<G> {
    /// A sealed server and client over real UDP, connected. `loss` is the client's outbound loss.
    fn connect(redundancy: usize, loss: f64) -> Self {
        let server = NetServer::with_secret(
            UdpTransport::bind(localhost()).expect("bind server"),
            ServerConfig {
                crypto: CryptoMode::Required,
                ..ServerConfig::default()
            },
            [3; 32],
        );
        let server_addr = server.local_addr().expect("server address");
        let largest_out = Rc::new(Cell::new(0));
        let blackout = Rc::new(Cell::new(false));
        let socket = Watched {
            inner: UdpTransport::bind(localhost()).expect("bind client"),
            largest_out: largest_out.clone(),
            blackout: blackout.clone(),
        };
        let client = NetClient::connect(
            socket,
            server_addr,
            ClientConfig {
                name: "n1".into(),
                input_redundancy: redundancy,
                crypto: CryptoMode::Required,
                conditioner: ConditionerConfig {
                    enabled: loss > 0.0,
                    seed: 0x5EED,
                    loss,
                    ..ConditionerConfig::default()
                },
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("join fits");
        let mut rig = Self {
            server,
            client,
            largest_out,
            blackout,
            now: 0,
            simulated: BTreeMap::new(),
            gaps: Vec::new(),
        };
        for _ in 0..2_000 {
            if rig.client.is_connected() && rig.server.session().len() == 1 {
                break;
            }
            rig.step();
        }
        assert!(rig.client.is_connected(), "connected: {:?}", rig.client.state());
        assert!(rig.client.is_sealed(), "the link is sealed, as shipped");
        assert_eq!(rig.server.session().len(), 1);
        rig
    }

    /// One 10 ms step of both endpoints; the server simulates every tick it has input for.
    fn step(&mut self) {
        self.now += STEP_MS;
        self.client.update(self.now);
        self.server.update(self.now);
        let Some(peer) = self.client.peer_id() else {
            return;
        };
        while self.server.driver().all_ready_for(self.server.driver().tick()) {
            let inputs = self.server.advance_tick(|_, _| {});
            let tick = self.server.driver().tick() - 1;
            match inputs.get(&peer) {
                Some(input) => {
                    self.simulated.insert(tick, input.clone());
                }
                None => self.gaps.push(tick),
            }
        }
    }

    /// Steps until the server simulated through `tick`, at most `max_steps`.
    fn run_until_simulated(&mut self, tick: u64, max_steps: usize) {
        for _ in 0..max_steps {
            if self.server.driver().tick() > tick {
                return;
            }
            self.step();
        }
    }
}

/// A 548-byte sample unique to `tick`.
fn order(tick: u64) -> Vec<u8> {
    sample(ORDER_PAYLOAD, tick)
}

fn sample(len: usize, tick: u64) -> Vec<u8> {
    let seed = tick.to_le_bytes();
    (0..len).map(|i| seed[i % 8] ^ (i as u8)).collect()
}

/// DO27 NET_N1B: 548-byte samples at redundancy 15 behind a 1200-byte cap. Every send is
/// accepted, no datagram exceeds the cap, and the server simulates every tick with its own sample.
#[test]
fn large_samples_at_redundancy_15_fit_the_datagram_and_every_tick_arrives() {
    assert_eq!(
        LinkConfig::default().max_packet_bytes,
        PACKET_CAP,
        "the cap DO27 runs"
    );
    let mut rig = Rig::<Orders>::connect(15, 0.0);
    for tick in 0..90u64 {
        rig.client
            .send_input(tick, order(tick))
            .unwrap_or_else(|e| panic!("tick {tick}: {e}"));
        rig.step();
    }
    rig.run_until_simulated(89, 200);
    assert!(rig.gaps.is_empty(), "ticks without input: {:?}", rig.gaps);
    let want: BTreeMap<u64, Vec<u8>> = (0..90).map(|t| (t, order(t))).collect();
    assert_eq!(rig.simulated, want, "every tick, its own sample, in order");
    assert!(
        rig.largest_out.get() <= PACKET_CAP,
        "largest datagram {} > {PACKET_CAP}",
        rig.largest_out.get()
    );
    let totals = rig.client.totals();
    assert!(
        totals.input_bundles_trimmed > 0,
        "the history was cut to the budget"
    );
    assert_eq!(totals.reliable_inputs, 0, "548 bytes fit a datagram");
    let budget = rig.client.input_bundle_budget().expect("connected");
    assert_eq!(
        budget, 1153,
        "1200 - sealed header - packet header - record header - lane byte"
    );
}

/// Under 10 % client loss the redundancy the extra bundles keep still delivers every tick of a
/// large-sample stream, in order.
#[test]
fn large_samples_survive_ten_percent_loss() {
    let mut rig = Rig::<Orders>::connect(15, 0.10);
    for tick in 0..150u64 {
        rig.client
            .send_input(tick, order(tick))
            .unwrap_or_else(|e| panic!("tick {tick}: {e}"));
        rig.step();
    }
    rig.run_until_simulated(119, 400);
    assert!(rig.gaps.is_empty(), "ticks without input: {:?}", rig.gaps);
    for tick in 0..120u64 {
        assert_eq!(rig.simulated.get(&tick), Some(&order(tick)), "tick {tick}");
    }
    let lost = rig.client.conditioner().stats();
    assert!(lost.dropped > 0, "the conditioner dropped packets: {lost:?}");
    assert!(rig.largest_out.get() <= PACKET_CAP);
}

/// Small samples keep the full redundancy: a blackout of 14 consecutive sends heals from the
/// 15th, which still carries all 15 inputs in one datagram.
#[test]
fn small_samples_keep_full_redundancy() {
    let mut rig = Rig::<Orders>::connect(15, 0.0);
    for tick in 0..40u64 {
        // Ticks 10..=23 never leave the client.
        rig.blackout.set((10..24).contains(&tick));
        rig.client.send_input(tick, sample(16, tick)).expect("queued");
        rig.step();
    }
    rig.blackout.set(false);
    rig.run_until_simulated(39, 200);
    assert!(rig.gaps.is_empty(), "ticks without input: {:?}", rig.gaps);
    for tick in 0..40u64 {
        assert_eq!(rig.simulated.get(&tick), Some(&sample(16, tick)), "tick {tick}");
    }
    let totals = rig.client.totals();
    assert_eq!(totals.input_bundles_trimmed, 0, "no small bundle was cut");
}

/// A refused send leaves the history as it was: the stream goes on, and the next bundle still
/// carries the inputs from before the refusal.
#[test]
fn a_refused_input_does_not_poison_the_history() {
    let mut rig = Rig::<Orders>::connect(15, 0.0);
    // Ticks 0..=4 never leave the client; only redundancy can deliver them.
    rig.blackout.set(true);
    for tick in 0..5u64 {
        rig.client.send_input(tick, sample(16, tick)).expect("queued");
        rig.step();
    }
    rig.blackout.set(false);
    let too_large = sample(MAX_RELIABLE_INPUT_BYTES + 1, 5);
    match rig.client.send_input(5, too_large) {
        Err(SendError::TooLarge { max, .. }) => assert_eq!(max, MAX_RELIABLE_INPUT_BYTES),
        other => panic!("expected TooLarge, got {other:?}"),
    }
    rig.client
        .send_input(5, sample(16, 5))
        .expect("the next send is not blocked");
    rig.step();
    for tick in 6..12u64 {
        rig.client.send_input(tick, sample(16, tick)).expect("queued");
        rig.step();
    }
    rig.run_until_simulated(11, 200);
    assert!(rig.gaps.is_empty(), "ticks without input: {:?}", rig.gaps);
    for tick in 0..12u64 {
        assert_eq!(rig.simulated.get(&tick), Some(&sample(16, tick)), "tick {tick}");
    }
}

/// One input larger than a datagram (4 KiB) takes the reliable input lane and arrives intact on
/// its tick, between small inputs on the unreliable lane, under 10 % loss.
#[test]
fn a_4_kib_input_arrives_intact_on_its_tick() {
    let mut rig = Rig::<Orders>::connect(15, 0.10);
    let big = sample(4096, 10);
    for tick in 0..40u64 {
        let input = if tick == 10 { big.clone() } else { sample(16, tick) };
        rig.client
            .send_input(tick, input)
            .unwrap_or_else(|e| panic!("tick {tick}: {e}"));
        rig.step();
    }
    rig.run_until_simulated(29, 600);
    assert!(rig.gaps.is_empty(), "ticks without input: {:?}", rig.gaps);
    assert_eq!(rig.simulated.get(&10), Some(&big), "intact and on tick 10");
    for tick in (0..30u64).filter(|t| *t != 10) {
        assert_eq!(rig.simulated.get(&tick), Some(&sample(16, tick)), "tick {tick}");
    }
    assert_eq!(rig.client.totals().reliable_inputs, 1);
    assert_eq!(rig.server.totals().reliable_inputs, 1);
    assert!(
        rig.largest_out.get() <= PACKET_CAP,
        "the reliable channel fragmented it"
    );
}

/// Several inputs in a row too large for a datagram, mixed with DO27-sized ones: every tick
/// arrives with its own sample.
#[test]
fn a_run_of_oversized_inputs_keeps_every_tick() {
    let mut rig = Rig::<Orders>::connect(15, 0.0);
    let input = |tick: u64| match tick % 3 {
        0 => sample(3000, tick),
        1 => order(tick),
        _ => sample(16, tick),
    };
    for tick in 0..30u64 {
        rig.client.send_input(tick, input(tick)).expect("queued");
        rig.step();
    }
    rig.run_until_simulated(29, 400);
    assert!(rig.gaps.is_empty(), "ticks without input: {:?}", rig.gaps);
    for tick in 0..30u64 {
        assert_eq!(rig.simulated.get(&tick), Some(&input(tick)), "tick {tick}");
    }
    assert_eq!(rig.client.totals().reliable_inputs, 10);
}

/// A reliable input that does not decode as the server's input type is refused and counted as
/// malformed against that peer; the tick gets no input from it.
#[test]
fn a_malformed_reliable_input_is_refused() {
    let mut rig = Rig::<HostileOrders>::connect(15, 0.0);
    // `len u16 = 0xFFFF` and 1998 bytes: too large for a datagram, so it travels reliably, and
    // too short for the length it claims.
    let hostile = RawInput(vec![0xFF; 2000]);
    rig.client.send_input(0, hostile).expect("queued");
    for _ in 0..50 {
        rig.step();
    }
    assert_eq!(rig.client.totals().reliable_inputs, 1);
    assert_eq!(rig.server.totals().reliable_inputs, 0, "not accepted");
    assert!(rig.server.totals().malformed >= 1, "{:?}", rig.server.totals());
    assert!(rig.simulated.is_empty(), "no tick ran on a refused input");
    // The hostile-input event of ADR-7892, for this peer, and not fatal: the peer stays.
    let peer = rig.client.peer_id().expect("still connected");
    let rejected: Vec<RejectKind> = std::iter::from_fn(|| rig.server.poll_event())
        .filter_map(|e| match e {
            ServerEvent::Rejected { peer: p, kind, .. } if p == peer => Some(kind),
            _ => None,
        })
        .collect();
    assert_eq!(rejected, vec![RejectKind::InputMalformed]);
    assert_eq!(rig.server.session().len(), 1);
}

/// A peer built before the reliable input lane (wire version 2) is refused at the handshake with
/// `protocol_mismatch`, before any key exchange or input.
#[test]
fn a_wire_version_2_peer_is_refused_cleanly() {
    // Wire 4 (ADR-8077), 5 (ADR-8184 transport v2, ADR-8298 resume ticket) and 6 (ADR-8858, the
    // one-round-trip resume) moved the current version on; a wire-2 peer is still refused.
    assert_eq!(regia_net::REGIA_NET_WIRE_VERSION, 6);
    let mut server = NetServer::<Orders, UdpTransport>::with_secret(
        UdpTransport::bind(localhost()).expect("bind server"),
        ServerConfig {
            crypto: CryptoMode::Required,
            ..ServerConfig::default()
        },
        [3; 32],
    );
    let server_addr = server.local_addr().expect("address");
    // The protocol id a wire-version-2 build of the same game computes (`protocol_id`).
    let mut w = WireWriter::new();
    w.u32(2);
    w.raw(Orders::GAME_ID.as_bytes());
    w.u8(0);
    w.u32(Orders::PROTOCOL_VERSION);
    let old_id = fnv1a_64(w.as_slice());
    assert_ne!(old_id, regia_net::protocol_id::<Orders>());
    let mut old = UdpTransport::bind(localhost()).expect("bind old client");
    let request = ConnectRequest {
        protocol_id: old_id,
        client_nonce: 0xA11CE,
        crypto_version: 2,
        crypto_required: true,
    };
    old.send_to(&request.to_datagram(), server_addr).expect("send");
    let mut deny = None;
    let mut buf = [0u8; 2048];
    for step in 1..=500u64 {
        server.update(step * STEP_MS);
        if let Some((n, _)) = old.recv_from(&mut buf).expect("receive")
            && let Some((PacketKind::Deny, body)) = split_datagram(&buf[..n])
        {
            deny = Some(Deny::decode(body).expect("deny"));
            break;
        }
    }
    let deny = deny.expect("the server answered");
    assert_eq!(deny.client_nonce, 0xA11CE);
    assert_eq!(deny.reason, DenyReason::ProtocolMismatch);
    assert_eq!(deny.reason.as_str(), "protocol_mismatch");
    assert_eq!(server.session().len(), 0, "nobody joined");
}
