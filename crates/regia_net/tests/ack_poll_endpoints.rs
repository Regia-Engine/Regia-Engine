// SPDX-License-Identifier: MPL-2.0
//! Sealed endpoint regressions for spec 120 § 5.3, with fixed steps and in-memory sockets.
use regia_net::prelude::*;
use regia_net::{CryptoMode, LinkConfig, LoopbackNet, LoopbackSocket};
use std::net::SocketAddr;

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = Vec<u8>;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.ack_poll";
    const PROTOCOL_VERSION: u32 = 1;
}

type Server = NetServer<Game, LoopbackSocket>;
type Client = NetClient<Game, LoopbackSocket>;

fn connected() -> (Server, Client, u64) {
    let net = LoopbackNet::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], 7000));
    let link = LinkConfig {
        max_packets_per_poll: 1,
        ..LinkConfig::default()
    };
    let mut server = NetServer::with_secret(
        net.bind(addr).expect("bind"),
        ServerConfig {
            crypto: CryptoMode::Required,
            tick_rate_hz: 60,
            tick_sync_interval_ms: 1,
            link: link.clone(),
            ..ServerConfig::default()
        },
        [79; 32],
    );
    let mut client = NetClient::connect(
        net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
        addr,
        ClientConfig {
            crypto: CryptoMode::Required,
            input_redundancy: 1,
            link,
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("join");
    for now in 0..100 {
        client.update(now);
        server.update(now);
        if client.is_connected() {
            client.update(now);
            return (server, client, now);
        }
    }
    panic!("handshake failed");
}

#[test]
fn essential_inputs_and_tick_sync_pass_a_reliable_backlog() {
    let (mut server, mut client, now) = connected();
    let peer = client.peer_id().expect("connected");
    for _ in 0..10 {
        server.advance_tick(|_, _| {});
    }
    for _ in 0..64 {
        server
            .send(peer, Channel::GAME_RPC, &vec![9; 1149])
            .expect("queued");
        client.send(Channel::GAME_RPC, &vec![8; 1149]).expect("queued");
    }
    client.send_input(12, vec![1, 2, 3]).expect("input queued");
    client.flush(now + 1);
    server.receive(now + 1);
    assert!(
        server.driver().has_input(peer, 12),
        "essential input is packed in the first poll"
    );
    server.flush(now + 1);
    client.receive(now + 1);
    assert!(
        client.estimated_server_tick(now + 1) >= 10,
        "tick sync is packed despite the backlog"
    );
}

#[test]
fn future_input_rejections_are_counted_on_the_endpoint_and_peer() {
    let (mut server, mut client, now) = connected();
    client.send_input(64, vec![1]).expect("unreliable input queued");
    client
        .send_input(65, vec![2; 4096])
        .expect("reliable input queued");
    for step in 1..30 {
        client.update(now + step);
        server.update(now + step);
    }
    let status = ron::to_string(&server.status()).expect("serialize");
    let peers = ron::to_string(&server.peer_rows()).expect("serialize");
    assert!(status.contains("inputs_too_far_ahead:2"), "endpoint: {status}");
    assert!(peers.contains("inputs_too_far_ahead:2"), "peers: {peers}");
    assert!(
        !server
            .driver()
            .has_input(client.peer_id().expect("connected"), 64)
    );
}
