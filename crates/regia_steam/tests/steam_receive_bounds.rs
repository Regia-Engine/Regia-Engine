// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 10.8 / adversarial row 15 (ADR-7981): a `SteamSocket` never hands on a message longer than
//! `MAX_DATAGRAM_BYTES`, and a send Steam refuses is an error, not `Ok`. Public API only, so the test runs unchanged
//! against the code before the bound.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use regia_net::DatagramSocket;
use regia_net::socket::MAX_DATAGRAM_BYTES;
use regia_steam::backend::{SteamError, SteamId};
use regia_steam::sockets::steam_peer_addr;
use regia_steam::{P2pMessages, SteamAccept, SteamSocket};

/// A platform queue of messages; every send is refused.
struct Platform {
    waiting: Mutex<VecDeque<(SteamId, Vec<u8>)>>,
}

impl P2pMessages for Platform {
    fn local_id(&self) -> Option<SteamId> {
        Some(SteamId(1))
    }
    fn send(&self, _to: SteamId, _channel: u16, _bytes: &[u8]) -> Result<(), SteamError> {
        Err(SteamError::Rejected("no session".into()))
    }
    fn receive(&self, _channel: u16, max: usize) -> Vec<(SteamId, Vec<u8>)> {
        let mut q = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        let n = max.min(q.len());
        q.drain(..n).collect()
    }
    fn set_accept(&self, _accept: SteamAccept) {}
}

fn socket(waiting: Vec<(SteamId, Vec<u8>)>) -> SteamSocket {
    let platform = Arc::new(Platform {
        waiting: Mutex::new(waiting.into_iter().collect()),
    });
    SteamSocket::open(platform, 0).expect("open")
}

#[test]
fn an_oversize_message_is_never_handed_on() {
    let mut s = socket(vec![
        (SteamId(5), vec![1; 10]),
        (SteamId(6), vec![2; 600_000]),
        (SteamId(5), vec![3; MAX_DATAGRAM_BYTES]),
        (SteamId(6), vec![4; MAX_DATAGRAM_BYTES + 1]),
    ]);
    let mut buf = vec![0u8; 65_536];
    let mut got = Vec::new();
    while let Some((n, from)) = s.recv_from(&mut buf).expect("recv") {
        got.push((from, n, buf[0]));
    }
    let from5 = steam_peer_addr(SteamId(5), 0);
    assert_eq!(got, vec![(from5, 10, 1), (from5, MAX_DATAGRAM_BYTES, 3)]);
}

#[test]
fn a_refused_send_is_an_error() {
    let mut s = socket(Vec::new());
    assert!(s.send_to(b"hello", steam_peer_addr(SteamId(9), 0)).is_err());
    assert_eq!(s.stats().send_errors, 1);
}
