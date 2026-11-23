// SPDX-License-Identifier: MPL-2.0
//! Review rows 2–4 and 6: actual connection packets over seeded virtual-time links.
use std::collections::HashMap;
use std::net::SocketAddr;

use regia_net::ack::AckFrame;
use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::connection::{Channel, Connection, LinkConfig};
use regia_net::wire::{WireReader, WireWriter};

/// The default link with the first flight named off (ADR-10156; off by default in v0.7,
/// ADR-10231, and a later release may turn it on): these
/// tests measure the transport's blind start (the 64-packet initial window and what it makes of
/// acknowledgments, bursts and rates), which the first flight replaces until a path answers.
fn blind_link() -> LinkConfig {
    let mut config = LinkConfig::default();
    config.rate.first_flight = false;
    config
}

/// The id of the first record of a wire-5 packet body when it is a reliable fragment.
fn first_reliable_id(p: &[u8]) -> Option<u16> {
    let mut r = WireReader::new(p);
    let flags = r.u8().ok()?;
    r.take([1, 2, 4, 4][usize::from(flags & 0x03)]).ok()?;
    if flags & 0x04 != 0 {
        AckFrame::decode(&mut r, flags & 0x08 != 0).ok()?;
    }
    if r.u8().ok()? & 0x7f != 0x02 {
        return None;
    }
    r.u16().ok()
}

struct Rig {
    a: Connection,
    b: Connection,
    down: NetConditioner,
    up: NetConditioner,
    received: usize,
}

fn delayed(rtt: u64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        delay_ms: rtt / 2,
        seed: 73,
        ..Default::default()
    }
}

impl Rig {
    fn new(config: ConditionerConfig) -> Self {
        Self {
            a: Connection::new(blind_link()),
            b: Connection::new(blind_link()),
            down: NetConditioner::new(config.clone()),
            up: NetConditioner::new(ConditionerConfig { seed: 97, ..config }),
            received: 0,
        }
    }

    fn step(&mut self, now: u64) {
        self.step_with_receiver_poll(now, true);
    }

    fn step_with_receiver_poll(&mut self, now: u64, poll_receiver: bool) {
        let addr = SocketAddr::from(([127, 0, 0, 1], 1));
        for p in self.a.poll(now, &[]) {
            for (p, _) in self.down.process(p, addr, now) {
                self.b.receive(&p, now).expect("packet");
            }
        }
        for (p, _) in self.down.release_due(now) {
            self.b.receive(&p, now).expect("packet");
        }
        if poll_receiver {
            for p in self.b.poll(now, &[]) {
                for (p, _) in self.up.process(p, addr, now) {
                    self.a.receive(&p, now).expect("ack");
                }
            }
        }
        for (p, _) in self.up.release_due(now) {
            self.a.receive(&p, now).expect("ack");
        }
        self.received += std::iter::from_fn(|| self.b.next_message()).count();
    }
}

fn rate_case(rtt: u64) {
    let mut r = Rig::new(delayed(rtt));
    let mut offered = 0;
    for tick in 0..1_200u64 {
        let now = tick * 1000 / 60;
        if tick < 600 {
            for _ in 0..3 {
                r.a.send(Channel::Unreliable, vec![7; 1188], 0).expect("queue");
                offered += 1;
            }
        }
        r.step(now);
    }
    println!(
        "RATE rtt={rtt} offered={offered} delivered={} pps={} lost={} expired={} resends={}",
        r.received,
        r.received as f64 / 10.0,
        r.a.stats().packets_lost,
        r.a.stats().unreliable_dropped,
        r.a.stats().reliable_resends
    );
    assert_eq!(
        r.received, offered,
        "180 packets/s must not be limited to 32 per RTT"
    );
    assert_eq!(r.a.stats().packets_lost, 0);
}

#[test]
fn rate_180pps_at_250ms_rtt() {
    rate_case(250);
}

#[test]
fn rate_180pps_at_3000ms_rtt() {
    rate_case(3000);
}

#[test]
fn reorder_only_does_not_count_as_loss() {
    let mut r = Rig::new(ConditionerConfig {
        reorder: 0.15,
        reorder_max_ms: 40,
        ..delayed(250)
    });
    for tick in 0..1_260u64 {
        if tick < 1_200 {
            r.a.send(Channel::Unreliable, vec![7; 100], 0).expect("queue");
        }
        r.step(tick * 1000 / 60);
    }
    println!(
        "REORDER delivered={} lost={} fraction={} injected={}",
        r.received,
        r.a.stats().packets_lost,
        r.a.stats().loss_fraction,
        r.down.stats().dropped
    );
    assert_eq!(r.received, 1_200);
    assert_eq!(r.a.stats().packets_lost, 0, "delivered reorder is not loss");
    assert_eq!(r.a.stats().loss_fraction, 0.0);
}

#[test]
fn wan_loss_tracks_injected_loss() {
    let mut r = Rig::new(ConditionerConfig::from_profile("wan").expect("profile"));
    for tick in 0..3_060u64 {
        if tick < 3_000 {
            r.a.send(Channel::Unreliable, vec![7; 100], 0).expect("queue");
        }
        r.step(tick * 1000 / 60);
    }
    let stats = r.a.stats();
    let injected = r.down.stats().dropped;
    println!(
        "WAN sent={} delivered={} lost={} injected={} fraction={}",
        stats.packets_sent, r.received, stats.packets_lost, injected, stats.loss_fraction
    );
    assert!(
        stats.packets_lost <= injected + 2,
        "reorder must not inflate injected loss"
    );
    assert!(
        stats.packets_lost + 2 >= injected,
        "real loss must still be detected"
    );
}

#[test]
fn a_2500ms_link_measures_its_rtt() {
    let mut r = Rig::new(delayed(2500));
    for tick in 0..1_000 {
        r.step(tick * 20);
    }
    println!(
        "RTT measured={} lost={}",
        r.a.stats().rtt_ms,
        r.a.stats().packets_lost
    );
    assert!((2400.0..2700.0).contains(&r.a.stats().rtt_ms));
    assert_eq!(r.a.stats().packets_lost, 0);
}

#[test]
fn hostile_zero_masks_cannot_resend_a_fragment_more_than_once_per_rtt() {
    let mut r = Rig::new(delayed(250));
    for now in (0..2000).step_by(5) {
        r.step(now);
    }
    let rtt = r.a.stats().rtt_ms.floor() as u64;
    assert!(rtt >= 250);
    for _ in 0..256 {
        r.a.send(Channel::GAME_RPC, vec![9; 1186], 0).expect("queue");
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], 1));
    let mut ack_seq = 400u16;
    let mut last = HashMap::new();
    let mut min_spacing = u64::MAX;
    let mut max_resends = 0;
    let start_bytes = r.a.stats().bytes_sent;
    for now in (2000..7000).step_by(5) {
        let mut resends = 0;
        for p in r.a.poll(now, &[]) {
            if let Some(id) = first_reliable_id(&p)
                && let Some(previous) = last.insert(id, now)
            {
                min_spacing = min_spacing.min(now - previous);
                resends += 1;
            }
            r.down.process(p, addr, now);
        }
        max_resends = max_resends.max(resends);
        let delivered = r.down.release_due(now);
        if let Some((newest, _)) = delivered.last() {
            // Authenticated attacker reports only the largest packet (wire 5: an acknowledgment
            // frame with a single one-packet range).
            let largest = u64::from(u16::from_le_bytes([newest[1], newest[2]]));
            let mut w = WireWriter::new();
            w.u8(0x01 | 0x04);
            w.u16(ack_seq);
            AckFrame {
                largest,
                delay_us: 0,
                arrival_ms: 0,
                ranges: vec![(largest, largest)],
                timestamps: Vec::new(),
            }
            .encode(&mut w, false, usize::MAX);
            ack_seq = ack_seq.wrapping_add(1);
            r.up.process(w.finish(), addr, now);
        }
        for (p, _) in r.up.release_due(now) {
            r.a.receive(&p, now).expect("ack");
        }
    }
    let bytes = r.a.stats().bytes_sent - start_bytes;
    println!(
        "HOSTILE bytes={bytes} resends={} min_spacing={min_spacing} max_per_poll={max_resends} rtt={rtt}",
        r.a.stats().reliable_resends
    );
    assert!(
        min_spacing >= rtt,
        "a fragment can be resent at most once per RTT"
    );
    assert!(
        max_resends <= 8,
        "loss-driven retransmissions are bounded per poll"
    );
    let resend_cap = 16 * 5000u64.div_ceil(rtt);
    assert!(
        r.a.stats().reliable_resends <= resend_cap,
        "per-peer RTT budget bounds amplification"
    );
    // Initial 256 packets, bounded retries, plus up to one small ACK-only packet per poll.
    assert!(bytes <= (256 + resend_cap) * 1200 + 1000 * 9);
}

#[test]
fn coalesced_bursts_preserve_the_older_ack_window_at_250ms_rtt() {
    let mut r = Rig::new(delayed(250));
    r.a = Connection::new(LinkConfig {
        resend_min_ms: 1000,
        ..blind_link()
    });
    for now in 0..600 {
        if now < 2 {
            for _ in 0..32 {
                r.a.send(Channel::GAME_RPC, vec![7; 1186], 0).expect("queue");
            }
        }
        // Both bursts arrive before the receiver's next poll; the older report must survive.
        r.step_with_receiver_poll(now, now.is_multiple_of(10));
        if now == 140 {
            assert_eq!(r.received, 64, "both bursts arrive before a return RTT");
        }
    }
    assert_eq!(r.received, 64);
    assert!(!r.a.has_unacked_reliable());
    assert_eq!(r.a.stats().packets_lost, 0);
    assert_eq!(r.a.stats().reliable_resends, 0);
}

#[test]
fn an_rtt_step_from_250_to_3000ms_does_not_freeze_the_estimator() {
    let mut r = Rig::new(delayed(250));
    for now in (0..10_000).step_by(10) {
        r.step(now);
    }
    assert!((250.0..280.0).contains(&r.a.stats().rtt_ms));
    r.down.set_config(delayed(3000));
    r.up.set_config(delayed(3000));
    for now in (10_000..40_000).step_by(10) {
        r.step(now);
    }
    println!(
        "RTT_STEP measured={} lost={}",
        r.a.stats().rtt_ms,
        r.a.stats().packets_lost
    );
    assert!((2900.0..3100.0).contains(&r.a.stats().rtt_ms));
    assert_eq!(r.a.stats().packets_lost, 0);
}

#[test]
fn conditioned_reorder_across_sequence_wrap_corrects_loss() {
    let mut r = Rig::new(ConditionerConfig {
        reorder: 0.15,
        reorder_max_ms: 40,
        ..delayed(250)
    });
    for tick in 0..70_100u64 {
        if tick < 70_000 {
            r.a.send(Channel::Unreliable, vec![7; 100], 0).expect("queue");
        }
        r.step(tick * 17);
    }
    assert!(r.a.stats().packets_sent > u64::from(u16::MAX));
    assert_eq!(r.received, 70_000);
    assert_eq!(r.a.stats().packets_lost, 0);
    assert_eq!(r.a.stats().loss_fraction, 0.0);
}

#[test]
fn a_reliable_transfer_bootstraps_on_a_3000ms_link() {
    let mut r = Rig::new(delayed(3000));
    r.a.send(Channel::GAME_RPC, vec![7; 256 * 1024], 0)
        .expect("queue");
    for now in (0..20_000).step_by(17) {
        r.step(now);
    }
    assert_eq!(r.received, 1);
    assert!(!r.a.has_unacked_reliable());
    assert!((2900.0..3100.0).contains(&r.a.stats().rtt_ms));
    assert_eq!(r.a.stats().packets_lost, 0);
}

#[test]
fn a_stalled_receivers_old_ack_reports_do_not_raise_smoothed_rtt() {
    let mut r = Rig::new(delayed(250));
    for now in (0..2000).step_by(20) {
        r.step(now);
    }
    let before = r.a.stats().rtt_ms;
    // The receiver keeps receiving but its application does not poll for ten seconds.
    // Its preserved ACK windows must not turn that application delay into path latency.
    for now in (2000..12_000).step_by(20) {
        r.step_with_receiver_poll(now, false);
    }
    for now in (12_000..12_160).step_by(20) {
        r.step(now);
    }
    println!("ACK_STALL before={before} after={}", r.a.stats().rtt_ms);
    assert!(
        (r.a.stats().rtt_ms - before).abs() <= 20.0,
        "coalesced stale ACK reports are not fresh RTT observations"
    );
}
