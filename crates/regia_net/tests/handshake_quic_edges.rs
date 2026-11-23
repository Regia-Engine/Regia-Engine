// SPDX-License-Identifier: MPL-2.0
//! Deterministic transport-budget integration tests for spec 120 § 11.6 (ADR-7946).
//! The in-memory socket changes its advertised limit at the exact send that is refused.

mod common;

use std::cell::Cell;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use common::{Duel, DuelMsg, Team};
use regia_net::crypto::CryptoMode;
use regia_net::prelude::*;
use regia_net::{LinkConfig, LoopbackSocket};

struct LimitedSocket {
    inner: LoopbackSocket,
    advertised: Rc<Cell<usize>>,
    actual: usize,
    refused: Rc<Cell<usize>>,
}

impl DatagramSocket for LimitedSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if bytes.len() > self.actual {
            self.advertised.set(self.actual);
            self.refused.set(self.refused.get() + 1);
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "datagram too big"));
        }
        self.inner.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.inner.recv_from(buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_datagram_size(&self, _dest: SocketAddr) -> usize {
        self.advertised.get()
    }
}

#[test]
fn both_endpoints_reschedule_a_size_refusal_and_deliver_reliable_messages() {
    for crypto in [CryptoMode::Off, CryptoMode::Required] {
        let net = LoopbackNet::new();
        let server_addr = SocketAddr::from(([10, 0, 0, 1], 7777));
        let client_addr = SocketAddr::from(([10, 0, 0, 2], 8888));
        let server_refused = Rc::new(Cell::new(0));
        let client_refused = Rc::new(Cell::new(0));
        let socket = |addr, refused| LimitedSocket {
            inner: net.bind(addr).expect("bind"),
            advertised: Rc::new(Cell::new(1400)),
            actual: 1200,
            refused,
        };
        let link = LinkConfig {
            max_packet_bytes: 1400,
            ..LinkConfig::default()
        };
        let mut server = NetServer::<Duel, _>::with_secret(
            socket(server_addr, server_refused.clone()),
            ServerConfig {
                crypto,
                link: link.clone(),
                ..ServerConfig::default()
            },
            [7; 32],
        );
        let mut client = NetClient::<Duel, _>::connect(
            socket(client_addr, client_refused.clone()),
            server_addr,
            ClientConfig {
                crypto,
                link,
                ..ClientConfig::default()
            },
            &Team(1),
        )
        .expect("connect");
        for step in 0..10 {
            client.update(step * 10);
            server.update(step * 10);
        }
        assert!(client.is_connected());
        let peer = server.session().peers().next().expect("peer").id;
        let text = "x".repeat(1300);
        client
            .send(Channel::GAME_RPC, &DuelMsg::Chat(text.clone()))
            .expect("client send");
        server
            .send(peer, Channel::GAME_RPC, &DuelMsg::Chat(text.clone()))
            .expect("server send");
        let (mut client_got, mut server_got) = (Vec::new(), Vec::new());
        for step in 10..100 {
            client.update(step * 10);
            server.update(step * 10);
            while let Some(event) = client.poll_event() {
                if let ClientEvent::Message {
                    message: DuelMsg::Chat(text),
                    ..
                } = event
                {
                    client_got.push(text);
                }
            }
            while let Some(event) = server.poll_event() {
                if let ServerEvent::Message {
                    message: DuelMsg::Chat(text),
                    ..
                } = event
                {
                    server_got.push(text);
                }
            }
        }
        assert_eq!(client_refused.get(), 1);
        assert_eq!(server_refused.get(), 1);
        assert_eq!(client_got.as_slice(), std::slice::from_ref(&text));
        assert_eq!(server_got, [text]);
        assert!(client.is_connected());
    }
}

#[test]
fn boxed_socket_forwards_the_payload_budget() {
    let net = LoopbackNet::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let socket: Box<dyn DatagramSocket> = Box::new(LimitedSocket {
        inner: net.bind(addr).expect("bind"),
        advertised: Rc::new(Cell::new(1156)),
        actual: 1156,
        refused: Rc::new(Cell::new(0)),
    });
    assert_eq!(socket.max_datagram_size(addr), 1156);
}

#[test]
fn a_known_small_budget_frames_large_reliable_messages_before_sending() {
    let net = LoopbackNet::new();
    let refused = Rc::new(Cell::new(0));
    let socket = LimitedSocket {
        inner: net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("local"),
        advertised: Rc::new(Cell::new(1156)),
        actual: 1156,
        refused: refused.clone(),
    };
    // The socket rejects every over-limit datagram; known limits must prevent any refusal.
    let mut server = NetServer::<Duel, _>::with_secret(
        socket,
        ServerConfig {
            crypto: CryptoMode::Off,
            ..ServerConfig::default()
        },
        [8; 32],
    );
    let server_addr = server.local_addr().expect("server address");
    let mut client = NetClient::<Duel, _>::connect(
        Box::new(net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("client")),
        server_addr,
        ClientConfig {
            crypto: CryptoMode::Off,
            ..ClientConfig::default()
        },
        &Team(1),
    )
    .expect("connect");
    for step in 0..10 {
        client.update(step * 10);
        server.update(step * 10);
    }
    let peer = server.session().peers().next().expect("peer").id;
    server
        .send(peer, Channel::GAME_RPC, &DuelMsg::Chat("large".repeat(700)))
        .expect("send");
    for step in 10..30 {
        server.update(step * 10);
        client.update(step * 10);
    }
    assert_eq!(refused.get(), 0, "known cap must apply before framing");
    let mut got = false;
    while let Some(event) = client.poll_event() {
        if let ClientEvent::Message {
            message: DuelMsg::Chat(text),
            ..
        } = event
        {
            assert_eq!(text, "large".repeat(700));
            got = true;
        }
    }
    assert!(got);
}
