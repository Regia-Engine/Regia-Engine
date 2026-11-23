// SPDX-License-Identifier: MPL-2.0
//! Socket send drops (spec 120 § 4.3, ADR-8407): a datagram the socket refuses (`WouldBlock`, a
//! full send buffer) is counted as `socket_drops` on the peer and the endpoint, not as sent, and
//! spends no packet number, so the receiver sees no gap and the sender declares no loss. In-memory
//! sockets, fixed steps, no wall clock.
use regia_net::prelude::*;
use regia_net::socket::DatagramSocket;
use regia_net::{CryptoMode, LinkConfig, LoopbackNet, LoopbackSocket};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = Vec<u8>;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.socket_drops";
    const PROTOCOL_VERSION: u32 = 1;
}

/// A loopback socket whose send buffer is "full" while `full` is set: every send is
/// `WouldBlock`, as a non-blocking UDP socket reports it.
struct Choke {
    inner: LoopbackSocket,
    full: Arc<AtomicBool>,
    refused: Arc<AtomicU64>,
}

impl DatagramSocket for Choke {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if self.full.load(Ordering::Relaxed) {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "send buffer full"));
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

type Server = NetServer<Game, LoopbackSocket>;
type Client = NetClient<Game, Choke>;

fn connected(crypto: CryptoMode) -> (Server, Client, Arc<AtomicBool>, Arc<AtomicU64>, u64) {
    let net = LoopbackNet::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], 7000));
    let mut server = NetServer::with_secret(
        net.bind(addr).expect("bind"),
        ServerConfig {
            crypto,
            tick_rate_hz: 60,
            link: LinkConfig::default(),
            ..ServerConfig::default()
        },
        [41; 32],
    );
    let full = Arc::new(AtomicBool::new(false));
    let refused = Arc::new(AtomicU64::new(0));
    let socket = Choke {
        inner: net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
        full: full.clone(),
        refused: refused.clone(),
    };
    let mut client = NetClient::connect(
        socket,
        addr,
        ClientConfig {
            crypto,
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("join");
    for now in 0..200 {
        client.update(now);
        server.update(now);
        if client.is_connected() {
            return (server, client, full, refused, now);
        }
    }
    panic!("handshake failed");
}

fn link(client: &Client) -> regia_net::connection::LinkStats {
    client.status().link.expect("connected link")
}

/// A full send buffer for a few polls: the client's reliable messages wait instead of being
/// counted as sent, the refusals are `socket_drops`, the server sees no gap in the packet
/// numbers, and nothing is declared lost or resent when the buffer drains.
fn wouldblock_is_a_socket_drop(crypto: CryptoMode) {
    let (mut server, mut client, full, refused, start) = connected(crypto);
    let out_before = client.totals().datagrams_out;
    let lost_before = link(&client).packets_lost;
    full.store(true, Ordering::Relaxed);
    for i in 0..5u8 {
        client.send(Channel::GAME_RPC, &vec![i; 200]).expect("queued");
    }
    for now in start + 1..start + 6 {
        client.flush(now);
        server.receive(now);
    }
    let refusals = refused.load(Ordering::Relaxed);
    assert!(
        refusals >= 1,
        "the socket refused the client's packets: {refusals}"
    );
    assert_eq!(
        client.totals().datagrams_out,
        out_before,
        "a refused datagram is not counted as sent"
    );
    full.store(false, Ordering::Relaxed);
    let mut got = Vec::new();
    for now in start + 6..start + 2_000 {
        client.update(now);
        server.update(now);
        while let Some(event) = server.poll_event() {
            if let ServerEvent::Message { message, .. } = event {
                got.push(message[0]);
            }
        }
    }
    assert_eq!(
        got,
        vec![0, 1, 2, 3, 4],
        "every message arrived once the buffer drained"
    );
    let l = link(&client);
    assert_eq!(
        l.packets_lost, lost_before,
        "a datagram that never left is not a loss: the packet number was not spent"
    );
    assert_eq!(l.reliable_resends, 0, "nothing was resent");
    assert!(l.socket_drops >= 1, "the peer counts its socket drops: {l:?}");
    assert_eq!(
        client.totals().socket_drops,
        refusals,
        "the endpoint counts every refused datagram"
    );
}

#[test]
fn wouldblock_is_a_socket_drop_not_a_send() {
    wouldblock_is_a_socket_drop(CryptoMode::Off);
}

#[test]
fn wouldblock_is_a_socket_drop_on_a_sealed_link() {
    wouldblock_is_a_socket_drop(CryptoMode::Required);
}
