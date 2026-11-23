// SPDX-License-Identifier: MPL-2.0
//! CPU cost of a `regia_net` server tick with many clients, over the in-memory loopback network
//! (no kernel sockets, so it measures this crate's own work: framing, acks, reliability, the
//! conditioner and the tick driver).
//!
//! ```text
//! cargo run --release -p regia_net --example net_bench -- [clients] [state_bytes] [ticks] [--sealed]
//! ```
//!
//! `--sealed` encrypts every link (ADR-1901), so a run with and a run without it give the cost of
//! the record layer; the join is not part of the measured ticks.
//!
//! Each tick every client sends one input bundle, the server advances one tick and sends every
//! client an unreliable state message of `state_bytes` plus a small reliable message every tenth
//! tick. Prints the mean and worst server time per tick (receive + tick + send + flush).

use std::net::SocketAddr;
use std::time::Instant;

use regia_net::CryptoMode;
use regia_net::prelude::*;

struct Bench;

impl NetGame for Bench {
    type Message = Vec<u8>;
    type Input = u32;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.example.bench";
    const PROTOCOL_VERSION: u32 = 1;
}

fn main() {
    let args: Vec<u64> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let clients = args.first().copied().unwrap_or(32) as usize;
    let state_bytes = args.get(1).copied().unwrap_or(900) as usize;
    let ticks = args.get(2).copied().unwrap_or(600);
    let crypto = if std::env::args().any(|a| a == "--sealed") {
        CryptoMode::Required
    } else {
        CryptoMode::Auto
    };
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let Ok(server_socket) = net.bind(server_addr) else {
        eprintln!("bind failed");
        return;
    };
    let mut server: NetServer<Bench, _> = NetServer::new(
        server_socket,
        ServerConfig {
            max_peers: clients,
            crypto,
            max_handshakes_per_s: 0,
            // Every simulated client shares one address.
            max_peers_per_ip: 0,
            handshakes_per_ip_per_s: 0,
            ..ServerConfig::default()
        },
    );
    let mut peers: Vec<NetClient<Bench, _>> = Vec::with_capacity(clients);
    for i in 0..clients {
        let Ok(socket) = net.bind(SocketAddr::from(([127, 0, 0, 1], 0))) else {
            eprintln!("bind failed");
            return;
        };
        let config = ClientConfig {
            name: format!("bot{i}"),
            crypto,
            ..ClientConfig::default()
        };
        match NetClient::connect(socket, server_addr, config, &()) {
            Ok(c) => peers.push(c),
            Err(e) => {
                eprintln!("connect: {e}");
                return;
            }
        }
    }
    let tick_ms = 1000 / u64::from(server.config().tick_rate_hz);
    let mut now = 0;
    for _ in 0..200 {
        now += 10;
        for c in &mut peers {
            c.update(now);
        }
        server.update(now);
        if peers.iter().all(|c| c.is_connected()) {
            break;
        }
    }
    if server.session().len() != clients {
        eprintln!("only {} of {clients} connected", server.session().len());
        return;
    }
    let state = vec![0xA5u8; state_bytes];
    let notice = vec![1u8; 64];
    let mut total_us = 0u128;
    let mut worst_us = 0u128;
    for t in 0..ticks {
        now += tick_ms;
        for c in &mut peers {
            c.receive(now);
            while c.poll_event().is_some() {}
            let tick = c.estimated_server_tick(now) + 2;
            let _ = c.send_input(tick, t as u32);
            c.flush(now);
        }
        let start = Instant::now();
        server.receive(now);
        while server.poll_event().is_some() {}
        server.advance_tick(|_, _| {});
        server.broadcast(Channel::Unreliable, &state);
        if t.is_multiple_of(10) {
            server.broadcast(Channel::GAME_RPC, &notice);
        }
        server.flush(now);
        let us = start.elapsed().as_micros();
        total_us += us;
        worst_us = worst_us.max(us);
    }
    let overlay = server.overlay();
    println!(
        "clients={clients} state_bytes={state_bytes} ticks={ticks} server_tick_mean_us={} server_tick_worst_us={worst_us} \
         datagrams_out={} max_rtt_ms={:.1}",
        total_us / u128::from(ticks.max(1)),
        server.totals().datagrams_out,
        overlay.max_rtt_ms
    );
}
