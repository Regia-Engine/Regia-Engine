// SPDX-License-Identifier: MPL-2.0
//! The reassembly stall deadline (spec 120 § 5.2, ADR-7981 amended by ADR-8521) against honest
//! senders that are slow for honest reasons (an outage, its recovery, lost repairs on a lossy
//! path) and against hostile senders that withhold a fragment for ever.
//!
//! Everything runs in virtual time on real `Connection`s or on hand-built wire 5 packets; no
//! sleeps, no wall-clock asserts.
use std::net::SocketAddr;

use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::connection::{
    Channel, Connection, LinkConfig, LinkFailure, REASSEMBLY_STALL_MIN_MS, REASSEMBLY_STALL_RTO_MULTIPLE,
};

/// The default link with the first flight named off (ADR-10156; off by default in v0.7,
/// ADR-10231, and a later release may turn it on): these
/// tests measure the transport's blind start (the 64-packet initial window and what it makes of
/// acknowledgments, bursts and rates), which the first flight replaces until a path answers.
fn blind_link() -> LinkConfig {
    let mut config = LinkConfig::default();
    config.rate.first_flight = false;
    config
}

/// A wire 5 packet body with a two-byte packet number (flags `0x01`), no acknowledgment frame,
/// and the given records.
fn packet(pn: u16, records: &[u8]) -> Vec<u8> {
    let mut p = vec![0x01];
    p.extend_from_slice(&pn.to_le_bytes());
    p.extend_from_slice(records);
    p
}

/// A reliable record: fragment `id`, `more` set when the message continues.
fn reliable(id: u16, more: bool, len: usize) -> Vec<u8> {
    let mut r = vec![if more { 0x82 } else { 0x02 }];
    r.extend_from_slice(&id.to_le_bytes());
    r.extend_from_slice(&(len as u16).to_le_bytes());
    r.extend(std::iter::repeat_n(0x5A, len));
    r
}

/// A ping record: an ack-eliciting keepalive.
const PING: [u8; 1] = [0x03];

/// The deadline with no measured loss, as ADR-7981 set it.
fn base_deadline(c: &Connection) -> u64 {
    (REASSEMBLY_STALL_RTO_MULTIPLE * c.resend_timeout_ms()).max(REASSEMBLY_STALL_MIN_MS)
}

/// A hostile peer stores fragments 1..=8 (never 0, the next expected one) and then sends a
/// keepalive every 100 ms; packet numbers advance by `pn_step`, and the peer goes silent for
/// `silence` ms after the first second. Returns the time the link failed, if it did within 60 s.
fn withholder(pn_step: u16, silence: u64) -> (Option<u64>, Connection) {
    let mut c = Connection::new(LinkConfig::default());
    let mut pn = 0u16;
    for id in 1..=8u16 {
        c.receive(&packet(pn, &reliable(id, true, 1_000)), 0)
            .expect("an out-of-order fragment is valid");
        pn = pn.wrapping_add(pn_step);
    }
    let mut now = 0;
    while now < 60_000 {
        now += 100;
        if now > 1_000 && now <= 1_000 + silence {
            continue;
        }
        if c.receive(&packet(pn, &PING), now).is_err() {
            return (Some(now), c);
        }
        pn = pn.wrapping_add(pn_step);
    }
    (None, c)
}

/// A hostile peer whose packets all arrive, in sequence, gets exactly the ADR-7981 deadline: the
/// loss stretch of ADR-8521 never applies to it.
#[test]
fn a_withheld_fragment_fails_the_link_at_the_base_deadline() {
    let (failed_at, c) = withholder(1, 0);
    let failed_at = failed_at.expect("the withholder must be failed");
    assert_eq!(c.failure(), Some(LinkFailure::ReassemblyStalled));
    // RTO is the configured minimum (nothing was ever acknowledged): the deadline is the 2 s floor.
    assert_eq!(base_deadline(&c), REASSEMBLY_STALL_MIN_MS);
    assert_eq!(failed_at, REASSEMBLY_STALL_MIN_MS, "failed at {failed_at} ms");
    assert_eq!(c.reassembly_stall_factor(), 1.0);
    assert_eq!(c.stats().reliable_buffered_bytes, 0, "freed");
}

/// A hostile peer that goes silent for 6 s (an outage it fakes) still fails within the base
/// deadline of its traffic: the silence counts one RTO, the rest of the deadline is traffic.
#[test]
fn a_withholder_that_goes_silent_still_fails_within_the_base_deadline() {
    let (failed_at, c) = withholder(1, 6_000);
    let failed_at = failed_at.expect("the withholder must be failed");
    assert_eq!(c.failure(), Some(LinkFailure::ReassemblyStalled));
    // 1,000 ms of traffic, one RTO (100 ms) for the silence (first packet at 7,100 ms), then
    // 900 ms of traffic after it.
    assert_eq!(failed_at, 7_100 + 900, "failed at {failed_at} ms");
}

/// A hostile peer that skips packet numbers claims a lossy path. That stretches the deadline, but
/// never past four times the base deadline (`REASSEMBLY_STALL_MAX_ATTEMPTS`), whatever it skips.
#[test]
fn a_withholder_that_fakes_loss_gains_at_most_four_times_the_deadline() {
    for step in [2u16, 3, 50, 1_000] {
        let (failed_at, c) = withholder(step, 0);
        let failed_at = failed_at.unwrap_or_else(|| panic!("step {step}: the withholder must be failed"));
        assert_eq!(c.failure(), Some(LinkFailure::ReassemblyStalled), "step {step}");
        assert!(
            failed_at <= 4 * REASSEMBLY_STALL_MIN_MS,
            "step {step}: failed at {failed_at} ms, past 4 x the base deadline"
        );
        assert!(
            failed_at > REASSEMBLY_STALL_MIN_MS,
            "step {step}: the claimed loss stretched it"
        );
    }
    // Half the numbers missing: an honest sender could lose 8 or more repairs in a row with a
    // chance above 1e-5, so the stretch is the full 4x.
    let (failed_at, _) = withholder(2, 0);
    assert_eq!(failed_at, Some(4 * REASSEMBLY_STALL_MIN_MS));
}

/// Two real connections, 50 ms each way, both polling every 10 ms; the receiver sends a small
/// reliable message every poll, so its every packet asks for an acknowledgment. The packet
/// carrying the first fragment of a three-fragment message is lost, then a 6 s outage cuts both
/// ways before the sender hears of the loss. The sender's acknowledgment of the receiver's last
/// packet before the outage only leaves after it, so the receiver's first round-trip sample
/// carries the outage; that sample must not turn the outage into stall time (ADR-8521). The honest
/// sender, recovering from the outage, repairs the gap and the message arrives; nobody fails.
#[test]
fn an_honest_sender_backed_off_by_a_6_s_outage_is_not_stalled_out() {
    const ONE_WAY: u64 = 50;
    const MESSAGE_AT: u64 = 1_050;
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    let mut to_b: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut to_a: Vec<(u64, Vec<u8>)> = Vec::new();
    let message: Vec<u8> = (0..3_000u32).map(|i| (i % 251) as u8).collect();
    let (outage_from, outage_to) = (1_160u64, 7_160u64);
    let mut dropped_first = false;
    let mut got = None;
    let mut worst_stall = 0u64;
    for now in (0..40_000u64).step_by(10) {
        let out = now >= outage_from && now < outage_to;
        if now < 20_000 {
            // Both sides keep reliable traffic going, so each acknowledges the other.
            b.send(Channel::GAME_RPC, vec![1; 40], 0).expect("queued");
            if now.is_multiple_of(100) {
                a.send(Channel::GAME_RPC, vec![2; 40], 0).expect("queued");
            }
        }
        if now == MESSAGE_AT {
            a.send(Channel::GAME_RPC, message.clone(), 0).expect("queued");
        }
        for p in a.poll(now, &[]) {
            if now == MESSAGE_AT && !dropped_first && p.len() > 1_000 {
                // The packet with the message's first fragment.
                dropped_first = true;
                continue;
            }
            if !out {
                to_b.push((now + ONE_WAY, p));
            }
        }
        for p in b.poll(now, &[]) {
            if !out {
                to_a.push((now + ONE_WAY, p));
            }
        }
        for (wire, end) in [(&mut to_b, &mut b), (&mut to_a, &mut a)] {
            let mut due: Vec<Vec<u8>> = Vec::new();
            wire.retain(|(at, p)| {
                if *at <= now {
                    due.push(p.clone());
                    false
                } else {
                    true
                }
            });
            for p in due {
                end.receive(&p, now).expect("honest traffic never fails the link");
            }
        }
        worst_stall = worst_stall.max(b.stats().reassembly_stall_ms);
        while let Some((channel, m)) = b.next_message() {
            if channel == Channel::GAME_RPC && m.len() == message.len() {
                got = Some(m);
            }
        }
        while a.next_message().is_some() {}
        if got.is_some() && now > outage_to + 2_000 {
            break;
        }
    }
    assert!(dropped_first, "the first fragment's packet was dropped");
    assert_eq!(got.as_deref(), Some(&message[..]), "the message arrives whole");
    assert!(a.failure().is_none() && b.failure().is_none());
    assert!(
        worst_stall < base_deadline(&b),
        "the outage counted {worst_stall} ms of stall"
    );
}

/// Seeded impaired links (the crate's conditioner: delay, jitter, loss, duplicates, reordering)
/// with one 6 s two-way outage, carrying reliable messages up to 20 KB both ways: every message
/// arrives once and in order and no honest link ever stalls out. On the lossier seeds the stall
/// passes the unstretched deadline at least once, so the loss stretch is what keeps them alive.
#[test]
fn honest_impaired_links_with_a_6_s_outage_never_stall_out() {
    let to = SocketAddr::from(([10, 0, 0, 1], 1));
    let mut passed_base = 0;
    for seed in 1..=6u64 {
        let link = |s: u64| {
            NetConditioner::new(ConditionerConfig {
                enabled: true,
                seed: s,
                loss: 0.05 + 0.04 * seed as f64,
                duplicate: 0.01,
                reorder: 0.10,
                reorder_max_ms: 60,
                delay_ms: 40 + 30 * seed,
                jitter_ms: 40,
                ..ConditionerConfig::default()
            })
        };
        let mut up = link(seed * 2);
        let mut down = link(seed * 2 + 1);
        let mut ends = [Connection::new(blind_link()), Connection::new(blind_link())];
        let (mut sent, mut got) = ([0u32; 2], [0u32; 2]);
        let outage = 3_000 + 500 * seed;
        let mut over_base = false;
        let mut now = 0u64;
        while now < 150_000 {
            let out = now >= outage && now < outage + 6_000;
            if now < 12_000 && now.is_multiple_of(150) {
                for (side, end) in ends.iter_mut().enumerate() {
                    let len = match (now / 150 + side as u64 + seed) % 5 {
                        0 => 20_000,
                        1 => 3_000,
                        _ => 60,
                    };
                    let mut m = vec![0u8; len];
                    m[..4].copy_from_slice(&sent[side].to_le_bytes());
                    if end.send(Channel::GAME_RPC, m, 0).is_ok() {
                        sent[side] += 1;
                    }
                }
            }
            for side in 0..2 {
                let polled = ends[side].poll(now, &[]);
                let wire = if side == 0 { &mut up } else { &mut down };
                let mut arrived = Vec::new();
                for p in polled {
                    if !out {
                        arrived.extend(wire.process(p, to, now));
                    }
                }
                arrived.extend(wire.release_due(now));
                let peer = &mut ends[1 - side];
                for (p, _) in arrived {
                    if let Err(e) = peer.receive(&p, now) {
                        panic!(
                            "seed {seed} at {now} ms: an honest link failed ({e}): {:?}",
                            peer.stats()
                        );
                    }
                }
                let stats = peer.stats();
                over_base |= stats.reassembly_stall_ms >= base_deadline(peer);
                while let Some((_, m)) = peer.next_message() {
                    let n = u32::from_le_bytes([m[0], m[1], m[2], m[3]]);
                    assert_eq!(n, got[1 - side], "seed {seed}: once, in order");
                    got[1 - side] += 1;
                }
            }
            if now > 12_000
                && got[0] == sent[1]
                && got[1] == sent[0]
                && ends.iter().all(|c| !c.has_unacked_reliable())
            {
                break;
            }
            now += 10;
        }
        assert_eq!(
            (got[0], got[1]),
            (sent[1], sent[0]),
            "seed {seed}: everything arrives"
        );
        assert!(ends.iter().all(|c| c.failure().is_none()), "seed {seed}");
        passed_base += usize::from(over_base);
        println!("STALL seed {seed} sent {sent:?} over_base {over_base}");
    }
    assert!(
        passed_base > 0,
        "no seed stretched the deadline: the test lost its teeth"
    );
}
