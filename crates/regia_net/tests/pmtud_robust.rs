// SPDX-License-Identifier: MPL-2.0
//! Path MTU discovery is not fooled (spec 120 § 4.3, ADR-8300): an outage on a 1,280-byte path is
//! not a black hole (the size is back within a second of the path), random loss on a lossy path
//! never drops the size, and a path whose black hole sees only small data still falls back by
//! three lost confirmation probes. Seeded conditioner, virtual time in 1 ms steps, no sleeps.
use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::connection::{Channel, Connection, LinkConfig};
use regia_net::pmtud::MtuSearch;
use std::net::SocketAddr;

fn addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 1))
}

/// A probe on the wire (no endpoint header here): no acknowledgment frame, a ping, then padding.
fn is_probe(p: &[u8]) -> bool {
    let Some(&flags) = p.first() else {
        return false;
    };
    let at = 1 + match flags & 0x03 {
        0 => 1,
        1 => 2,
        _ => 4,
    };
    flags & 0x04 == 0 && p.get(at..at + 2) == Some(&[0x03, 0x00][..])
}

struct Pair {
    a: Connection,
    b: Connection,
    up: NetConditioner,
    down: NetConditioner,
    /// Largest datagram a sent that was not a probe, per 1 s bucket.
    a_largest: Vec<usize>,
}

impl Pair {
    fn new(delay_ms: u64, loss: f64, seed: u64) -> Self {
        let link = ConditionerConfig {
            enabled: true,
            delay_ms,
            loss,
            ..Default::default()
        };
        let config = LinkConfig {
            mtu_probing: true,
            ..LinkConfig::default()
        };
        Self {
            a: Connection::new(config.clone()),
            b: Connection::new(config),
            up: NetConditioner::new(ConditionerConfig { seed, ..link.clone() }),
            down: NetConditioner::new(ConditionerConfig {
                seed: seed + 1,
                ..link
            }),
            a_largest: vec![0; 128],
        }
    }

    /// One millisecond: `mtu` is the path MTU now (0 = an outage).
    fn step(&mut self, now: u64, mtu: usize) {
        for p in self.a.poll(now, &[]) {
            if !is_probe(&p) {
                let bucket = &mut self.a_largest[(now / 1000) as usize];
                *bucket = (*bucket).max(p.len());
            }
            if p.len() > mtu {
                continue;
            }
            for (p, _) in self.up.process(p, addr(), now) {
                self.b.receive(&p, now).expect("valid");
            }
        }
        for (p, _) in self.up.release_due(now) {
            self.b.receive(&p, now).expect("valid");
        }
        for p in self.b.poll(now, &[]) {
            if p.len() > mtu {
                continue;
            }
            for (p, _) in self.down.process(p, addr(), now) {
                self.a.receive(&p, now).expect("valid");
            }
        }
        for (p, _) in self.down.release_due(now) {
            self.a.receive(&p, now).expect("valid");
        }
    }

    /// a sends full-size state every 16 ms and a 300-byte reliable message every 50 ms; b a small
    /// input every 16 ms.
    fn traffic(&mut self, now: u64, large: bool) {
        if now.is_multiple_of(16) {
            let len = if large {
                self.a.max_unreliable_bytes(0)
            } else {
                100
            };
            let _ = self.a.send(Channel::Unreliable, vec![1; len], 0);
            let _ = self.b.send(Channel::Unreliable, vec![2; 40], 0);
        }
        if now.is_multiple_of(50) {
            let _ = self.a.send(Channel::GAME_RPC, vec![3; 300], 0);
        }
    }

    fn drain(&mut self) -> usize {
        let mut reliable = 0;
        while let Some((channel, _)) = self.b.next_message() {
            reliable += usize::from(channel == Channel::GAME_RPC);
        }
        while self.a.next_message().is_some() {}
        reliable
    }
}

/// A 5 s outage (both directions) on a 1,280-byte path: no black hole, and full-size data is back
/// within a second of the path returning.
#[test]
fn an_outage_is_not_a_black_hole() {
    let mut pair = Pair::new(20, 0.0, 11);
    let (mut sent, mut got) = (0, 0);
    for now in 0..40_000u64 {
        pair.traffic(now, true);
        sent += usize::from(now.is_multiple_of(50));
        let mtu = if (20_000..25_000).contains(&now) { 0 } else { 1280 };
        pair.step(now, mtu);
        got += pair.drain();
    }
    let s = pair.a.stats();
    println!(
        "outage: largest data per second 18-30 s {:?}; stats path_mtu {} {:?} probes {}/{} black holes {}",
        &pair.a_largest[18..30],
        s.path_mtu,
        s.mtu_search,
        s.mtu_probes_sent,
        s.mtu_probes_lost,
        s.mtu_black_holes
    );
    assert_eq!(s.mtu_black_holes, 0, "an outage is not a black hole");
    assert!(
        pair.a_largest[26] > 1200,
        "full size again within a second of the path"
    );
    assert_eq!(s.path_mtu, 1280);
    assert_eq!(got, sent, "every reliable message arrives");
}

/// 3 % random loss each way for 60 s on a 1,280-byte path: the size is never dropped.
#[test]
fn random_loss_never_drops_the_size() {
    for seed in [21u64, 22, 23] {
        let mut pair = Pair::new(30, 0.03, seed);
        for now in 0..60_000u64 {
            pair.traffic(now, true);
            pair.step(now, 1280);
            pair.drain();
        }
        let s = pair.a.stats();
        let small_seconds = pair.a_largest[5..60].iter().filter(|l| **l <= 1200).count();
        println!(
            "loss seed {seed}: path_mtu {} {:?} probes {}/{} black holes {} seconds without a large packet {small_seconds}",
            s.path_mtu, s.mtu_search, s.mtu_probes_sent, s.mtu_probes_lost, s.mtu_black_holes
        );
        assert_eq!(
            s.mtu_black_holes, 0,
            "seed {seed}: random loss is not a black hole"
        );
        assert_eq!(s.path_mtu, 1280, "seed {seed}");
        assert_eq!(small_seconds, 0, "seed {seed}: full-size data every second");
    }
}

/// Only small data on the path when it stops carrying 1,280 (t = 30 s): no large data packet is
/// lost, so the confirmation probes (every 5 s without a large acknowledgment) find the black
/// hole, by the third lost one.
#[test]
fn a_black_hole_under_small_traffic_is_found_by_three_lost_confirmations() {
    let mut pair = Pair::new(20, 0.0, 31);
    let mut at_hole = None;
    for now in 0..60_000u64 {
        pair.traffic(now, now < 10_000);
        pair.step(now, if now < 30_000 { 1280 } else { 1200 });
        pair.drain();
        let s = pair.a.stats();
        if s.mtu_black_holes == 1 && at_hole.is_none() {
            at_hole = Some((now, s.mtu_probes_lost));
        }
    }
    let s = pair.a.stats();
    println!(
        "small traffic: black hole at {at_hole:?}; stats {:?} {:?}",
        s.mtu_search, s
    );
    let (when, lost_total) = at_hole.expect("the black hole is found");
    assert!((30_000..50_000).contains(&when), "found at {when} ms");
    // Three 1,400 probes lost during the search, then three lost confirmations of 1,280.
    assert_eq!(lost_total, 6, "by the third lost confirmation");
    assert_eq!((s.path_mtu, s.mtu_search), (1200, MtuSearch::Error));
}
