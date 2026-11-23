// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 11.5 / adversarial row 14 (ADR-7981): the shared game port (`--shared-port`) never queues a datagram
//! longer than `MAX_DATAGRAM_BYTES` (no endpoint reads past it), and one half holds at most 4 MiB of datagrams. Counted
//! through the public `DatagramSocket` halves only, so the test runs unchanged against the code before the bound.

use std::net::SocketAddr;

use regia_net::handshake::{PacketKind, connected_header};
use regia_net::server_host::SharedPort;
use regia_net::socket::{DatagramSocket, LoopbackNet, MAX_DATAGRAM_BYTES};

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("address")
}

/// A connected payload from an address no handshake claimed: it goes to the session half.
fn payload(len: usize) -> Vec<u8> {
    let mut d = connected_header(PacketKind::Payload, 7).to_vec();
    d.resize(len, 0);
    d
}

/// Every datagram the session half delivers, read with a buffer as large as UDP allows.
fn drain(half: &mut dyn DatagramSocket) -> Vec<(usize, SocketAddr)> {
    let mut buf = vec![0u8; 65_536];
    std::iter::from_fn(|| half.recv_from(&mut buf).expect("recv")).collect()
}

#[test]
fn an_oversize_datagram_is_never_queued() {
    let net = LoopbackNet::new();
    let server = addr("10.20.0.1:7000");
    let (_port, mut session, _link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
    let mut peer = net.bind(addr("10.20.0.2:1")).expect("bind");
    peer.send_to(&payload(100), server).expect("send");
    for _ in 0..10 {
        peer.send_to(&payload(65_000), server).expect("send");
    }
    peer.send_to(&payload(MAX_DATAGRAM_BYTES), server).expect("send");
    let got: Vec<usize> = drain(&mut session).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        got,
        vec![100, MAX_DATAGRAM_BYTES],
        "the 65,000-byte datagrams are dropped"
    );
}

#[test]
fn a_half_holds_at_most_4_mib() {
    let net = LoopbackNet::new();
    let server = addr("10.20.1.1:7000");
    let (_port, mut session, mut link) = SharedPort::split(Box::new(net.bind(server).expect("bind")), 42);
    let mut flood = net.bind(addr("10.20.1.2:1")).expect("bind");
    for _ in 0..4096 {
        flood.send_to(&payload(MAX_DATAGRAM_BYTES), server).expect("send");
    }
    // Reading the link half pumps the socket into both queues; the session half then holds what was kept.
    assert!(drain(&mut link).is_empty());
    let held: usize = drain(&mut session).into_iter().map(|(n, _)| n).sum();
    assert!(held > 0);
    assert!(held <= 4 * 1024 * 1024, "the session half held {held} B");
}
