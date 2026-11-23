// SPDX-License-Identifier: MPL-2.0
//! `net_core_headless`: a headless authoritative server and two clients over the in-memory
//! loopback network, built from the ECS-free core of `regia_net` (spec 120 § 3.2, ADR-8032,
//! ADR-8501). Nothing here needs the `ecs` feature: the endpoints ([`NetServer`], [`NetClient`]),
//! the handshake, the tick driver ([`TickClock`] and the server's `TickDriver`), and net messages
//! declared with `#[derive(NetMessage)]` and carried through a [`NetMessageRegistry`] inside the
//! game's own payload. A game that keeps its own simulation (DO27's sim, a lockstep RTS) links
//! this and nothing of the ECS.
//!
//! ```text
//! cargo run -p regia_net --no-default-features --example net_core_headless
//! ```
//!
//! `cargo xtask check-net-core` builds it without the feature, and the `net_core_headless`
//! integration test runs [`run`] (time is a step counter, 10 ms a step; nothing sleeps).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::process::ExitCode;

use regia_net::NetMessage;
use regia_net::codec::{CodecContext, LayoutHasher, encode_to_vec};
use regia_net::message::NetMessageRegistry;
use regia_net::prelude::*;

/// Milliseconds per loop step.
pub const STEP_MS: u64 = 10;
/// The server's address on the loopback network.
const SERVER_PORT: u16 = 7777;
/// The server broadcasts the standings every this many ticks.
pub const SCORE_EVERY_TICKS: u64 = 10;
/// Most steps the clients get to connect.
const CONNECT_STEPS: u64 = 500;

/// The standings, broadcast by the server: a net message declared with the derive, encoded with
/// the replication codec, no ECS involved.
#[derive(Debug, Clone, PartialEq, NetMessage)]
#[net(direction = "broadcast", id = "example.net_core_headless.score")]
pub struct Score {
    /// The tick the standings are for.
    pub tick: u64,
    /// The players' peer ids, in peer order.
    pub peers: Vec<u32>,
    /// Each player's position, in the order of `peers`.
    pub positions: Vec<i32>,
}

/// One client's per-tick input: a step left or right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stride(pub i32);

impl NetCodec for Stride {
    fn encode(&self, w: &mut WireWriter) {
        w.i32(self.0);
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self(r.i32()?))
    }
}

/// The message types both ends register, in the same order (the order feeds the protocol hash).
pub fn registry() -> NetMessageRegistry {
    let mut r = NetMessageRegistry::default();
    r.register::<Score>();
    r
}

/// The example game: messages are a registry index plus codec bytes, inputs are strides.
pub struct Arena;

impl NetGame for Arena {
    type Message = Vec<u8>;
    type Input = Stride;
    type JoinData = ();
    const GAME_ID: &'static str = "example.net_core_headless";
    const PROTOCOL_VERSION: u32 = 1;

    /// The message registry's layout hash: a client built with another message set cannot join.
    fn content_hash() -> u64 {
        let mut h = LayoutHasher::new();
        registry().hash_into(&mut h);
        h.finish()
    }
}

/// Frames one registered message: its registry index (u16, little endian), then its codec bytes.
pub fn frame<M: NetMessage>(registry: &NetMessageRegistry, msg: &M) -> Option<Vec<u8>> {
    let index = registry.index_of::<M>()?;
    let mut bytes = index.to_le_bytes().to_vec();
    bytes.extend(encode_to_vec(msg, &CodecContext::detached()));
    Some(bytes)
}

/// Reads one framed message back as `M`; `None` when it is another type or does not decode.
pub fn unframe<M: NetMessage>(registry: &NetMessageRegistry, bytes: &[u8]) -> Option<M> {
    let (index, payload) = bytes.split_first_chunk::<2>()?;
    let index = u16::from_le_bytes(*index);
    let erased = registry.decode(index, payload, &CodecContext::detached()).ok()?;
    erased.downcast::<M>().ok().map(|m| *m)
}

/// What one run saw.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    /// The peer id of each client, in connect order.
    pub peers: Vec<PeerId>,
    /// Joins the server reported.
    pub joined: usize,
    /// Ticks the server simulated.
    pub ticks: u64,
    /// The server's positions at the end, by peer.
    pub positions: BTreeMap<PeerId, i32>,
    /// The standings each client received, in arrival order.
    pub scores: Vec<Vec<Score>>,
    /// Ticks the server simulated without some peer's input, summed over peers.
    pub missed_inputs: u64,
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Runs a session: `clients` clients connect, then each sends a stride every tick (client `k`
/// steps `+1` when `k` is even, `-1` when odd) for `ticks` server ticks while the server
/// simulates and broadcasts the standings every [`SCORE_EVERY_TICKS`] ticks.
pub fn run(clients: usize, ticks: u64) -> Result<Report, String> {
    let net = LoopbackNet::new();
    let server_socket = net
        .bind(addr(SERVER_PORT))
        .map_err(|e| format!("bind server: {e}"))?;
    let mut server: NetServer<Arena, _> =
        NetServer::with_secret(server_socket, ServerConfig::default(), [7; 32]);
    let mut endpoints = Vec::with_capacity(clients);
    for k in 0..clients {
        let socket = net.bind(addr(0)).map_err(|e| format!("bind client {k}: {e}"))?;
        let config = ClientConfig {
            name: format!("player{k}"),
            ..ClientConfig::default()
        };
        let client: NetClient<Arena, _> = NetClient::connect(socket, addr(SERVER_PORT), config, &())
            .map_err(|e| format!("connect client {k}: {e}"))?;
        endpoints.push(client);
    }
    let registry = registry();
    let mut report = Report {
        scores: vec![Vec::new(); clients],
        ..Report::default()
    };

    let mut now = 0;
    for _ in 0..CONNECT_STEPS {
        if endpoints.iter().all(NetClient::is_connected) {
            break;
        }
        now += STEP_MS;
        for c in &mut endpoints {
            c.update(now);
        }
        server.update(now);
    }
    if !endpoints.iter().all(NetClient::is_connected) {
        return Err(format!("not every client connected within {CONNECT_STEPS} steps"));
    }
    for c in &endpoints {
        report
            .peers
            .push(c.peer_id().ok_or("a connected client has a peer id")?);
    }

    let mut clock = TickClock::new(server.config().tick_rate_hz, 4);
    let mut positions: BTreeMap<PeerId, i32> = BTreeMap::new();
    while report.ticks < ticks {
        now += STEP_MS;
        for (k, c) in endpoints.iter_mut().enumerate() {
            c.receive(now);
            let stride = Stride(if k % 2 == 0 { 1 } else { -1 });
            for tick in c.due_input_ticks(now) {
                c.send_input(tick, stride)
                    .map_err(|e| format!("client {k} input: {e}"))?;
            }
            c.flush(now);
            while let Some(event) = c.poll_event() {
                if let ClientEvent::Message { message, .. } = event
                    && let Some(score) = unframe::<Score>(&registry, &message)
                {
                    report.scores[k].push(score);
                }
            }
        }
        server.receive(now);
        for _ in 0..clock.advance(STEP_MS) {
            server.advance_tick(|_, inputs| {
                for (peer, stride) in inputs {
                    *positions.entry(*peer).or_insert(0) += stride.0;
                }
            });
            report.ticks += 1;
            if report.ticks.is_multiple_of(SCORE_EVERY_TICKS) {
                let score = Score {
                    tick: server.driver().tick(),
                    peers: positions.keys().map(|p| p.0).collect(),
                    positions: positions.values().copied().collect(),
                };
                let bytes = frame(&registry, &score).ok_or("Score is registered")?;
                server.broadcast(Channel::GAME_RPC, &bytes);
            }
        }
        server.flush(now);
        while let Some(event) = server.poll_event() {
            if let ServerEvent::PeerJoined { .. } = event {
                report.joined += 1;
            }
        }
    }
    // Let the last standings arrive.
    for _ in 0..50 {
        now += STEP_MS;
        server.update(now);
        for (k, c) in endpoints.iter_mut().enumerate() {
            c.update(now);
            while let Some(event) = c.poll_event() {
                if let ClientEvent::Message { message, .. } = event
                    && let Some(score) = unframe::<Score>(&registry, &message)
                {
                    report.scores[k].push(score);
                }
            }
        }
    }
    report.missed_inputs = server.peer_rows().iter().map(|r| r.missed_input_ticks).sum();
    report.positions = positions;
    Ok(report)
}

/// Runs a two-client session of 300 ticks and prints what it saw.
pub fn main() -> ExitCode {
    match run(2, 300) {
        Ok(report) => {
            println!(
                "net_core_headless: {} clients, {} ticks, positions {:?}, standings received {:?}, missed inputs {}",
                report.peers.len(),
                report.ticks,
                report.positions,
                report.scores.iter().map(Vec::len).collect::<Vec<_>>(),
                report.missed_inputs
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("net_core_headless: {e}");
            ExitCode::FAILURE
        }
    }
}
