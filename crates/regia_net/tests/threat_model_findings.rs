// SPDX-License-Identifier: MPL-2.0
//! Open findings of the threat-model review in code this lane does not own (ADR-3206). Each test
//! fails today and passes once the finding is fixed; the number names it in the lane report.

mod common;

use std::net::SocketAddr;

use common::{Duel, Team};
use regia_net::crypto::CRYPTO_VERSION;
use regia_net::handshake::ConnectRequest;
use regia_net::prelude::*;
use regia_net::socket::DatagramSocket;
use regia_net::{LoopbackSocket, protocol_id};

const STEP_MS: u64 = 10;
const SERVER_PORT: u16 = 7777;

fn server_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], SERVER_PORT))
}

fn client_on(net: &LoopbackNet, at: SocketAddr, name: &str) -> NetClient<Duel, LoopbackSocket> {
    NetClient::connect(
        net.bind(at).expect("bind client"),
        server_addr(),
        ClientConfig {
            name: name.into(),
            ..ClientConfig::default()
        },
        &Team(0),
    )
    .expect("join fits")
}

/// Finding 23: the per-IP handshake budget is spent before the source address is proved, so anyone
/// forging a victim's address at the budget's rate keeps that victim from ever getting a cookie.
#[test]
#[ignore = "finding 23: open, routed by the lead (regia_net::server)"]
fn finding_23_forged_requests_do_not_keep_a_new_client_from_joining() {
    let net = LoopbackNet::new();
    let mut server: NetServer<Duel, LoopbackSocket> = NetServer::with_secret(
        net.bind(server_addr()).expect("bind server"),
        ServerConfig {
            handshakes_per_ip_per_s: 4,
            ..ServerConfig::default()
        },
        [7; 32],
    );
    // The forger shares the victim's IP in the source field: only the port differs here.
    let victim_ip = [127, 0, 0, 2];
    let mut forger = net
        .bind(SocketAddr::from((victim_ip, 6000)))
        .expect("bind forger");
    let request = ConnectRequest {
        protocol_id: protocol_id::<Duel>(),
        client_nonce: 99,
        crypto_version: CRYPTO_VERSION,
        crypto_required: false,
    }
    .to_datagram();
    // The flood comes first, so the source's budget is already spent when the victim asks.
    let flood_steps = 100_u64;
    for step in 0..flood_steps {
        for _ in 0..4 {
            forger.send_to(&request, server_addr()).expect("send");
        }
        server.update(step * STEP_MS);
    }
    let mut client = client_on(&net, SocketAddr::from((victim_ip, 5000)), "victim");
    for step in flood_steps..flood_steps + 500 {
        let now = step * STEP_MS;
        for _ in 0..4 {
            forger.send_to(&request, server_addr()).expect("send");
        }
        client.update(now);
        server.update(now);
        if client.is_connected() {
            return;
        }
    }
    panic!("five seconds of forged requests kept the client out");
}

/// Finding 28: only the handshake table groups IPv6 sources by /64. The per-address slot cap counts
/// exact addresses, so one /64 rotates addresses and takes every slot.
#[test]
#[ignore = "finding 28: open, routed by the lead (regia_net::server)"]
fn finding_28_one_ipv6_block_is_one_address_for_the_slot_cap() {
    let net = LoopbackNet::new();
    let listen: SocketAddr = "[2001:db8::ff]:7777".parse().expect("addr");
    let mut server: NetServer<Duel, LoopbackSocket> = NetServer::with_secret(
        net.bind(listen).expect("bind server"),
        ServerConfig {
            max_peers_per_ip: 1,
            ..ServerConfig::default()
        },
        [7; 32],
    );
    let mut clients: Vec<NetClient<Duel, LoopbackSocket>> = ["[2001:db8::1]:5000", "[2001:db8::2]:5000"]
        .iter()
        .map(|at| {
            NetClient::connect(
                net.bind(at.parse().expect("addr")).expect("bind client"),
                listen,
                ClientConfig::default(),
                &Team(0),
            )
            .expect("join fits")
        })
        .collect();
    for step in 0..200_u64 {
        let now = step * STEP_MS;
        for client in &mut clients {
            client.update(now);
        }
        server.update(now);
    }
    assert_eq!(server.session().len(), 1, "one /64 took more than one slot");
}
