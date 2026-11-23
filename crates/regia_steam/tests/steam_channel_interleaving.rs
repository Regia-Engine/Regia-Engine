// SPDX-License-Identifier: MPL-2.0
//! Interleaved channel and peer contract for the Steam mock transport.

use regia_net::DatagramSocket;
use std::sync::Arc;

use regia_steam::{
    MockSteam, MockSteamNet, SteamAccept, SteamBackend, SteamId, SteamSocket, steam_id_of, steam_peer_addr,
};

fn user(net: &MockSteamNet, id: u64, name: &str) -> Arc<MockSteam> {
    let mock = MockSteam::shared();
    mock.join_net(net, SteamId(id), name);
    mock
}

fn receive(socket: &mut SteamSocket, buf: &mut [u8]) -> Option<(Vec<u8>, std::net::SocketAddr)> {
    socket
        .recv_from(buf)
        .expect("receive succeeds")
        .map(|(len, from)| (buf[..len].to_vec(), from))
}

#[test]
fn interleaved_peers_channels_policy_and_online_state_remain_isolated() {
    let net = MockSteamNet::new();
    let a = user(&net, 101, "a");
    let b = user(&net, 202, "b");
    let c = user(&net, 303, "c");
    let a_p2p = a.p2p().expect("a p2p");
    let b_p2p = b.p2p().expect("b p2p");
    let c_p2p = c.p2p().expect("c p2p");

    for channel in [0, u16::MAX] {
        let addr = steam_peer_addr(SteamId(303), channel);
        assert_eq!(steam_id_of(addr), Some((SteamId(303), channel)));
    }

    let mut a_zero = SteamSocket::open(a_p2p.clone(), 0).expect("a channel zero");
    let mut a_max = SteamSocket::open(a_p2p.clone(), u16::MAX).expect("a max channel");
    let mut b_zero = SteamSocket::open(b_p2p.clone(), 0).expect("b channel zero");
    let mut c_zero = SteamSocket::open(c_p2p.clone(), 0).expect("c channel zero");

    // The mock owns one FIFO for each (receiver, channel). Interleave the senders and
    // backlog channel zero while channel u16::MAX remains independently drainable.
    for index in 0..35u8 {
        b_zero
            .send_to(&[b'b', index], steam_peer_addr(SteamId(101), 0))
            .expect("b to a");
        c_zero
            .send_to(&[b'c', index], steam_peer_addr(SteamId(101), 0))
            .expect("c to a");
    }
    let mut c_max = SteamSocket::open(c_p2p.clone(), u16::MAX).expect("c max channel");
    c_max
        .send_to(b"max-channel", steam_peer_addr(SteamId(101), u16::MAX))
        .expect("send on max channel");

    let mut buf = [0u8; 32];
    assert_eq!(
        receive(&mut a_max, &mut buf),
        Some((b"max-channel".to_vec(), steam_peer_addr(SteamId(303), u16::MAX)))
    );
    assert_eq!(a_max.stats().received, 1);

    // The first fill takes 64 datagrams into a_zero's private queue. Policy changes
    // cannot retroactively reject those; the next fill applies the then-current policy.
    assert_eq!(
        receive(&mut a_zero, &mut buf),
        Some((vec![b'b', 0], steam_peer_addr(SteamId(202), 0)))
    );
    for index in 1..32u8 {
        assert_eq!(
            receive(&mut a_zero, &mut buf),
            Some((vec![b'c', index - 1], steam_peer_addr(SteamId(303), 0)))
        );
        assert_eq!(
            receive(&mut a_zero, &mut buf),
            Some((vec![b'b', index], steam_peer_addr(SteamId(202), 0)))
        );
    }
    // Finish the 64-message socket batch before changing policy. Any later batch is
    // filtered when the platform queue is drained, not when send_to enqueues it.
    assert_eq!(
        receive(&mut a_zero, &mut buf),
        Some((vec![b'c', 31], steam_peer_addr(SteamId(303), 0)))
    );
    // Remaining shared-queue entries are evaluated by A's policy at the next fill.
    // (A is the receiver, so set its policy, not B's.)
    a_p2p.set_accept(SteamAccept::Listed([SteamId(202)].into_iter().collect()));
    let mut received_b = 0;
    while let Some((payload, from)) = receive(&mut a_zero, &mut buf) {
        assert_eq!(from, steam_peer_addr(SteamId(202), 0));
        assert_eq!(payload[0], b'b');
        received_b += 1;
    }
    assert_eq!(received_b, 3);
    assert_eq!(net.counters(), (68, 3));
    assert_eq!((a_zero.stats().received, a_zero.stats().receive_calls), (67, 3));
    assert_eq!((b_zero.stats().sent, b_zero.stats().send_errors), (35, 0));

    // A policy rejection is independent of send success. Restore Anyone for the control run.
    a_p2p.set_accept(SteamAccept::Anyone);
    c_zero
        .send_to(b"control", steam_peer_addr(SteamId(101), 0))
        .expect("control send");
    assert_eq!(
        receive(&mut a_zero, &mut buf),
        Some((b"control".to_vec(), steam_peer_addr(SteamId(303), 0)))
    );
    assert_eq!(net.counters(), (69, 3));
    assert_eq!((c_zero.stats().sent, c_zero.stats().send_errors), (36, 0));

    // Offline destinations fail at send time; restoring online permits a control datagram.
    net.set_online(SteamId(202), false);
    let mut c_to_b = SteamSocket::open(c_p2p, 0).expect("c socket");
    // Surfaced as an error and counted (spec 120 § 10.8, adversarial row 15).
    assert!(
        c_to_b
            .send_to(b"offline", steam_peer_addr(SteamId(202), 0))
            .is_err(),
        "an offline send is surfaced"
    );
    assert_eq!((c_to_b.stats().sent, c_to_b.stats().send_errors), (0, 1));
    net.set_online(SteamId(202), true);
    c_to_b
        .send_to(b"online", steam_peer_addr(SteamId(202), 0))
        .expect("online control");
    let mut b_buf = [0u8; 16];
    assert_eq!(
        receive(&mut b_zero, &mut b_buf),
        Some((b"online".to_vec(), steam_peer_addr(SteamId(303), 0)))
    );
    assert_eq!((c_to_b.stats().sent, c_to_b.stats().send_errors), (1, 1));
}

#[test]
fn receive_truncation_counts_the_datagram_once_and_reports_its_source() {
    let net = MockSteamNet::new();
    let a = user(&net, 401, "a");
    let b = user(&net, 402, "b");
    let mut sender = SteamSocket::open(b.p2p().expect("b p2p"), u16::MAX).expect("sender");
    let mut receiver = SteamSocket::open(a.p2p().expect("a p2p"), u16::MAX).expect("receiver");
    let payload = b"abcdefghij";
    sender
        .send_to(payload, steam_peer_addr(SteamId(401), u16::MAX))
        .expect("send");
    let mut short = [0u8; 4];
    assert_eq!(
        receive(&mut receiver, &mut short),
        Some((b"abcd".to_vec(), steam_peer_addr(SteamId(402), u16::MAX)))
    );
    assert_eq!(receive(&mut receiver, &mut short), None);
    assert_eq!((sender.stats().sent, sender.stats().send_errors), (1, 0));
    assert_eq!(
        (receiver.stats().received, receiver.stats().receive_calls),
        (1, 2)
    );
    assert_eq!(net.counters(), (1, 0));
}
