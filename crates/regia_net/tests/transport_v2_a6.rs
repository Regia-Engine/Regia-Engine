// SPDX-License-Identifier: MPL-2.0
//! DO27 rule A6 on transport v2 (spec 120 § 4.5, ADR-8184): no datagram is sent twice and no
//! packet number is reused. It reads the wire-5 layout and uses wire-5 API, so unlike
//! `transport_v2.rs` it does not build on the wire-4 base (where every packet also carried its
//! own sequence number; this is a guard, not a fail-before row).
use std::net::SocketAddr;

use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::connection::{Channel, Connection, LinkConfig};

fn addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 1))
}

fn stamp(kind: u8, index: u32, now: u64, len: usize) -> Vec<u8> {
    let mut m = vec![0u8; len.max(13)];
    m[0] = kind;
    m[1..5].copy_from_slice(&index.to_le_bytes());
    m[5..13].copy_from_slice(&now.to_le_bytes());
    m
}

fn link(delay_ms: u64, jitter_ms: u64, loss: f64, reorder: f64, reorder_max_ms: u64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        delay_ms,
        jitter_ms,
        loss,
        reorder,
        reorder_max_ms,
        ..Default::default()
    }
}

/// Moves `from`'s packets through `cond` into `to`, checking each datagram is new and its packet
/// number the next one. Returns the datagrams sent.
fn pump(
    side: usize,
    from: &mut Connection,
    to: &mut Connection,
    cond: &mut NetConditioner,
    seen: &mut std::collections::HashSet<Vec<u8>>,
    next_pn: &mut u64,
    now: u64,
) -> u64 {
    let mut sent = 0;
    for p in from.poll(now, &[]) {
        sent += 1;
        // Wire 5: flags, then the packet number (2 bytes while fewer than 16,384 wait).
        assert_eq!(p[0] & 0x03, 1, "a 2-byte packet number");
        let pn = u64::from(u16::from_le_bytes([p[1], p[2]]));
        assert_eq!(
            pn,
            *next_pn & 0xFFFF,
            "side {side}: packet numbers in order, none reused"
        );
        *next_pn += 1;
        assert!(
            seen.insert(p.clone()),
            "side {side}: a datagram was sent twice at {now} ms"
        );
        for (p, _) in cond.process(p, addr(), now) {
            to.receive(&p, now).expect("valid packet");
        }
    }
    for (p, _) in cond.release_due(now) {
        to.receive(&p, now).expect("valid packet");
    }
    sent
}

/// DO27 rule A6, "no duplicated datagrams on the wire" (spec 120 § 4.5): on a lossy path with
/// game traffic, small reliable messages (redundant copies), a 64-packet burst and repairs, no
/// datagram is sent twice and every packet number is used once, in order. Repairs and copies are
/// new packets, never repeated bytes.
#[test]
fn a6_no_datagram_is_sent_twice_and_no_packet_number_is_reused() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    let mut up = NetConditioner::new(ConditionerConfig {
        seed: 61,
        ..link(20, 5, 0.05, 0.02, 20)
    });
    let mut down = NetConditioner::new(ConditionerConfig {
        seed: 62,
        ..link(20, 5, 0.05, 0.02, 20)
    });
    let mut seen = [std::collections::HashSet::new(), std::collections::HashSet::new()];
    let mut next_pn = [0u64; 2];
    let (mut datagrams, mut copies_checked) = (0u64, 0u64);
    for now in 0..20_000u64 {
        if now % 16 == 0 {
            a.send(Channel::Unreliable, stamp(1, 0, now, 40), 0)
                .expect("input");
            b.send(Channel::Unreliable, stamp(1, 0, now, 300), 0)
                .expect("state");
        }
        if now % 100 == 0 {
            a.send(Channel::GAME_RPC, stamp(2, 0, now, 80), 0).expect("rpc");
        }
        if now == 5_000 {
            for i in 0..64u32 {
                b.send(Channel::GAME_RPC, stamp(3, i, now, 1_100), 0)
                    .expect("burst");
            }
        }
        datagrams += pump(0, &mut a, &mut b, &mut up, &mut seen[0], &mut next_pn[0], now);
        datagrams += pump(1, &mut b, &mut a, &mut down, &mut seen[1], &mut next_pn[1], now);
        while a.next_message().is_some() {}
        while b.next_message().is_some() {}
        copies_checked = a.stats().redundant_copies + b.stats().redundant_copies;
    }
    let resends = a.stats().reliable_resends + b.stats().reliable_resends;
    println!("A6 datagrams={datagrams} redundant_copies={copies_checked} resends={resends}");
    assert!(
        copies_checked > 0 && resends > 0,
        "the run exercised copies and repairs"
    );
    assert_eq!(next_pn[0], a.next_packet_number());
    assert_eq!(next_pn[1], b.next_packet_number());
}
