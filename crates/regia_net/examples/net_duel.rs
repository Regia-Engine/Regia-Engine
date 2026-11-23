// SPDX-License-Identifier: MPL-2.0
//! A tiny two-player game over `regia_net`: each player races a marker along a track; the server
//! owns the positions, applies the inputs it receives for each tick, broadcasts the state and
//! announces the winner.
//!
//! ```text
//! cargo run -p regia_net --example net_duel                 # server + two bots in one process,
//!                                                           # on the loopback network with the
//!                                                           # `wan` conditioner (no real sockets)
//! cargo run -p regia_net --example net_duel -- server 7777  # a real UDP server
//! cargo run -p regia_net --example net_duel -- client 127.0.0.1:7777 ada
//! cargo run -p regia_net --example net_duel -- client 192.0.2.7:7777 ada <server key>
//! ```
//!
//! The server prints its public key. A client on another machine encrypts by default (ADR-1901);
//! the key argument pins the server, so a client that reaches any other server refuses it.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use regia_net::ServerKey;
use regia_net::prelude::*;
use regia_net::send_clock::{is_send_tick, send_interval_ticks};

/// The game's network vocabulary.
struct Duel;

/// Track length; the first marker past it wins.
const FINISH: i32 = 60;

#[derive(Debug, Clone, PartialEq)]
enum DuelMsg {
    /// Positions at a tick.
    State { tick: u64, positions: Vec<(u16, i32)> },
    /// The winner's peer id.
    Winner(u16),
}

impl NetCodec for DuelMsg {
    fn encode(&self, w: &mut WireWriter) {
        match self {
            DuelMsg::State { tick, positions } => {
                w.u8(0);
                w.u64(*tick);
                w.u8(positions.len().min(255) as u8);
                for (peer, pos) in positions.iter().take(255) {
                    w.u16(*peer);
                    w.i32(*pos);
                }
            }
            DuelMsg::Winner(peer) => {
                w.u8(1);
                w.u16(*peer);
            }
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => {
                let tick = r.u64()?;
                let n = r.u8()?;
                let positions = (0..n)
                    .map(|_| Ok((r.u16()?, r.i32()?)))
                    .collect::<Result<_, WireError>>()?;
                Ok(DuelMsg::State { tick, positions })
            }
            1 => Ok(DuelMsg::Winner(r.u16()?)),
            _ => Err(WireError::Invalid("duel message")),
        }
    }
}

/// Stride this tick: 0, 1 or 2 cells.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stride(u8);

impl NetCodec for Stride {
    fn encode(&self, w: &mut WireWriter) {
        w.u8(self.0);
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            v @ 0..=2 => Ok(Stride(v)),
            _ => Err(WireError::Invalid("stride")),
        }
    }
}

impl NetGame for Duel {
    type Message = DuelMsg;
    type Input = Stride;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.example.duel";
    const PROTOCOL_VERSION: u32 = 1;
}

/// Server-side rules.
struct Track {
    positions: BTreeMap<PeerId, i32>,
    winner: Option<PeerId>,
}

impl Track {
    fn step(&mut self, inputs: &TickInputs<Stride>) {
        for (peer, stride) in inputs {
            let pos = self.positions.entry(*peer).or_insert(0);
            *pos += i32::from(stride.0);
            if *pos >= FINISH && self.winner.is_none() {
                self.winner = Some(*peer);
            }
        }
    }

    fn state(&self, tick: u64) -> DuelMsg {
        DuelMsg::State {
            tick,
            positions: self.positions.iter().map(|(p, x)| (p.0 as u16, *x)).collect(),
        }
    }
}

/// A bot's stride for a tick: a fixed, seeded pattern so runs repeat.
fn bot_stride(seed: u64, tick: u64) -> Stride {
    let mut x = seed ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 29;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    Stride(((x >> 32) % 3) as u8)
}

/// One step of server work: receive, run due ticks, send state, flush. Returns the winner once.
fn server_step<S: DatagramSocket>(
    server: &mut NetServer<Duel, S>,
    track: &mut Track,
    clock: &mut TickClock,
    now_ms: u64,
    elapsed_ms: u64,
) -> Option<PeerId> {
    server.receive(now_ms);
    while let Some(event) = server.poll_event() {
        match event {
            ServerEvent::PeerInterrupted { peer } => println!("server: {peer} interrupted"),
            ServerEvent::PeerRestored { peer } => println!("server: {peer} restored"),
            ServerEvent::PeerJoined { peer, name, .. } => println!("server: {name} joined as {peer}"),
            ServerEvent::PeerLeft { peer, reason } => println!("server: {peer} left ({})", reason.as_str()),
            ServerEvent::PeerDormant { peer, .. } => println!("server: {peer} dormant"),
            ServerEvent::PeerResumed { peer, name, .. } => println!("server: {name} resumed as {peer}"),
            ServerEvent::Message { .. } => {}
            ServerEvent::Rejected { peer, kind, error } => {
                println!("server: refused {} from {peer}: {error}", kind.as_str())
            }
            ServerEvent::QualityChanged { peer, from, to } => println!("server: {peer} link {from} -> {to}"),
        }
    }
    let interval = send_interval_ticks(clock.tick_rate_hz(), 15);
    let mut announced = None;
    if server.session().len() >= 2 && track.winner.is_none() {
        for _ in 0..clock.advance(elapsed_ms) {
            let tick = server.driver().tick();
            server.advance_tick(|_, inputs| track.step(inputs));
            if is_send_tick(tick, interval) {
                server.broadcast(Channel::Unreliable, &track.state(tick));
            }
            if let Some(w) = track.winner {
                server.broadcast(Channel::GAME_RPC, &DuelMsg::Winner(w.0 as u16));
                announced = Some(w);
                break;
            }
        }
    }
    server.flush(now_ms);
    announced
}

/// One step of client work: receive, send inputs for the ticks ahead, flush.
fn client_step<S: DatagramSocket>(
    client: &mut NetClient<Duel, S>,
    seed: u64,
    last_sent: &mut Option<u64>,
    now_ms: u64,
) -> Option<u16> {
    client.receive(now_ms);
    let mut winner = None;
    while let Some(event) = client.poll_event() {
        match event {
            ClientEvent::PeerInterrupted => println!("client: connection interrupted"),
            ClientEvent::PeerRestored => println!("client: connection restored"),
            ClientEvent::InputsLate { margin_p5 } => {
                println!("client: inputs late (p5 margin {margin_p5} ticks)")
            }
            ClientEvent::Connected { peer, tick_rate_hz } => {
                println!("client: connected as {peer} at {tick_rate_hz} Hz")
            }
            ClientEvent::Message {
                message: DuelMsg::Winner(w),
                ..
            } => winner = Some(w),
            ClientEvent::Message { .. } => {}
            ClientEvent::Ended(end) => println!("client: ended ({})", end.as_str()),
            ClientEvent::VersionMismatch { message, .. } => println!("client: {message}"),
            ClientEvent::QualityChanged { from, to } => println!("client: link {from} -> {to}"),
        }
    }
    if client.is_connected() {
        let target = client.estimated_server_tick(now_ms) + 6;
        let from = last_sent.map_or(target, |t| t + 1);
        for tick in from..=target {
            let _ = client.send_input(tick, bot_stride(seed, tick));
        }
        if from <= target {
            *last_sent = Some(target);
        }
    }
    client.flush(now_ms);
    winner
}

fn run_local() {
    let net = LoopbackNet::new();
    let server_addr: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let wan = ConditionerConfig::from_profile("wan").unwrap_or_default();
    let (Ok(server_socket), Ok(a_socket), Ok(b_socket)) = (
        net.bind(server_addr),
        net.bind(SocketAddr::from(([127, 0, 0, 1], 0))),
        net.bind(SocketAddr::from(([127, 0, 0, 1], 0))),
    ) else {
        eprintln!("loopback bind failed");
        return;
    };
    let mut server: NetServer<Duel, _> = NetServer::new(
        server_socket,
        ServerConfig {
            conditioner: wan.clone(),
            ..ServerConfig::default()
        },
    );
    let bot = |name: &str, seed: u64| ClientConfig {
        name: name.into(),
        conditioner: ConditionerConfig { seed, ..wan.clone() },
        ..ClientConfig::default()
    };
    let (Ok(mut a), Ok(mut b)) = (
        NetClient::<Duel, _>::connect(a_socket, server_addr, bot("ada", 2), &()),
        NetClient::<Duel, _>::connect(b_socket, server_addr, bot("bob", 3), &()),
    ) else {
        eprintln!("client setup failed");
        return;
    };
    let mut track = Track {
        positions: BTreeMap::new(),
        winner: None,
    };
    let mut clock = TickClock::new(server.config().tick_rate_hz, 4);
    let (mut a_sent, mut b_sent) = (None, None);
    let step_ms = 5;
    let mut won_at = None;
    for step in 1..=6_000u64 {
        let now = step * step_ms;
        let seen_a = client_step(&mut a, 11, &mut a_sent, now);
        let seen_b = client_step(&mut b, 22, &mut b_sent, now);
        if let Some(w) = seen_a.or(seen_b) {
            println!("client: told peer{w} won");
        }
        if let Some(w) = server_step(&mut server, &mut track, &mut clock, now, step_ms) {
            println!("server: {w} wins at tick {}", server.driver().tick());
            won_at = Some(now);
        }
        if step.is_multiple_of(400) {
            for line in server.overlay().lines() {
                println!("{line}");
            }
        }
        // Keep running a second after the win so the reliable announcement reaches both bots.
        if won_at.is_some_and(|t| now >= t + 1_000) {
            break;
        }
    }
    a.disconnect();
    b.disconnect();
    let end = 6_001 * step_ms;
    a.flush(end);
    b.flush(end);
    server.update(end);
    for line in server.overlay().lines() {
        println!("{line}");
    }
}

fn run_server(port: u16) {
    let socket = match UdpTransport::bind(SocketAddr::from(([0, 0, 0, 0], port))) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bind :{port}: {e}");
            return;
        }
    };
    let mut server: NetServer<Duel, _> = NetServer::new(socket, ServerConfig::default());
    println!("server: listening on {:?}", server.local_addr());
    if let Some(key) = server.server_key() {
        println!("server: key {key}");
    }
    let start = Instant::now();
    let mut last = 0;
    let mut track = Track {
        positions: BTreeMap::new(),
        winner: None,
    };
    let mut clock = TickClock::new(server.config().tick_rate_hz, 4);
    let mut done_at = None;
    while start.elapsed() < Duration::from_secs(120) {
        let now = start.elapsed().as_millis() as u64;
        if server_step(&mut server, &mut track, &mut clock, now, now - last).is_some() {
            done_at = Some(now);
        }
        last = now;
        if done_at.is_some_and(|t| now > t + 1_000) {
            server.shutdown();
            server.flush(now);
            let t = server.totals();
            println!(
                "server: done, {} handshakes, {} sealed out, {} opened, {} refused by the tag, {} replays",
                t.handshakes, t.sealed_out, t.opened_in, t.auth_failed, t.replayed
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_client(addr: &str, name: &str, key: Option<&str>) {
    let Ok(server_addr) = addr.parse::<SocketAddr>() else {
        eprintln!("not an address: {addr}");
        return;
    };
    let server_key = match key.map(ServerKey::parse) {
        Some(None) => {
            eprintln!("not a server key: {}", key.unwrap_or_default());
            return;
        }
        Some(pin) => pin,
        None => None,
    };
    let socket = match UdpTransport::bind(SocketAddr::from(([0, 0, 0, 0], 0))) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bind: {e}");
            return;
        }
    };
    let config = ClientConfig {
        name: name.into(),
        server_key,
        ..ClientConfig::default()
    };
    let Ok(mut client) = NetClient::<Duel, _>::connect(socket, server_addr, config, &()) else {
        eprintln!("name too long");
        return;
    };
    let seed = regia_net::wire::fnv1a_64(name.as_bytes());
    let start = Instant::now();
    let mut sent = None;
    let mut reported = false;
    while start.elapsed() < Duration::from_secs(120) {
        let now = start.elapsed().as_millis() as u64;
        if let Some(w) = client_step(&mut client, seed, &mut sent, now) {
            println!("client: winner is peer{w}");
        }
        if client.is_connected() && !reported {
            reported = true;
            match client.server_key() {
                Some(key) => println!("client: sealed link to {key}"),
                None => println!("client: plain link"),
            }
        }
        if matches!(client.state(), ClientState::Ended(_)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("local") => run_local(),
        Some("server") => match args.get(1).map_or(Ok(7777), |p| p.parse()) {
            Ok(port) => run_server(port),
            Err(_) => eprintln!("bad port {:?}; use a number from 0 to 65535", args[1]),
        },
        Some("client") => run_client(
            args.get(1).map_or("127.0.0.1:7777", String::as_str),
            args.get(2).map_or("player", String::as_str),
            args.get(3).map(String::as_str),
        ),
        Some(other) => {
            eprintln!("unknown mode {other}; use local, server [port] or client <addr> [name] [server key]")
        }
    }
}
