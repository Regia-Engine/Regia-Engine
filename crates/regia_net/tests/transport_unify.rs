// SPDX-License-Identifier: MPL-2.0
//! Unified transport policy (ADR-7995): loss detection from acknowledgment evidence, probe
//! pacing during silence, delivery-proportional in-flight window and outage recovery, over seeded
//! virtual-time links. Review rows 2, 3, 4, 6, 11 and 15 of sprint-net-rev-a.
//!
//! Every test drives real `Connection` packets through the crate's `NetConditioner` (or a fixed
//! delay line) with explicit millisecond clocks: no sleeps, no wall-clock asserts.
use std::collections::{HashMap, VecDeque};
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

/// The acknowledgment frame of a wire-5 packet body, if it carries one.
fn ack_frame(p: &[u8]) -> Option<AckFrame> {
    let mut r = WireReader::new(p);
    let flags = r.u8().ok()?;
    r.take([1, 2, 4, 4][usize::from(flags & 0x03)]).ok()?;
    if flags & 0x04 == 0 {
        return None;
    }
    AckFrame::decode(&mut r, flags & 0x08 != 0).ok()
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

fn addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 1))
}

/// Two connections joined by one conditioner per direction.
struct Rig {
    a: Connection,
    b: Connection,
    down: NetConditioner,
    up: NetConditioner,
    /// Messages `b` delivered, in order.
    got: Vec<(Channel, Vec<u8>)>,
    /// While true `b` neither receives nor polls: its datagrams wait (a stalled process whose
    /// socket buffer keeps filling).
    b_stalled: bool,
    b_backlog: Vec<Vec<u8>>,
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
            // Raw transport throughput: more reliable messages per second than a class cap allows
            // (spec 120 § 4.5 A2, ADR-8407), so the caps are off.
            a: Connection::new(LinkConfig {
                message_caps: false,
                ..blind_link()
            }),
            b: Connection::new(blind_link()),
            down: NetConditioner::new(config.clone()),
            up: NetConditioner::new(ConditionerConfig { seed: 97, ..config }),
            got: Vec::new(),
            b_stalled: false,
            b_backlog: Vec::new(),
        }
    }

    fn step(&mut self, now: u64) {
        for p in self.a.poll(now, &[]) {
            for (p, _) in self.down.process(p, addr(), now) {
                self.b_backlog.push(p);
            }
        }
        for (p, _) in self.down.release_due(now) {
            self.b_backlog.push(p);
        }
        if !self.b_stalled {
            for p in std::mem::take(&mut self.b_backlog) {
                self.b.receive(&p, now).expect("packet");
            }
            for p in self.b.poll(now, &[]) {
                for (p, _) in self.up.process(p, addr(), now) {
                    self.a.receive(&p, now).expect("ack");
                }
            }
            self.got.extend(std::iter::from_fn(|| self.b.next_message()));
        }
        for (p, _) in self.up.release_due(now) {
            self.a.receive(&p, now).expect("ack");
        }
    }

    fn reliable_got(&self) -> usize {
        self.got.iter().filter(|(c, _)| *c == Channel::GAME_RPC).count()
    }
}

/// Keeps the reliable queue of `a` full with 1,000-byte messages numbered from `next`.
fn feed_bulk(a: &mut Connection, next: &mut u32) {
    loop {
        let mut m = vec![0u8; 1000];
        m[..4].copy_from_slice(&next.to_le_bytes());
        if a.send(Channel::GAME_RPC, m, 0).is_err() {
            break;
        }
        *next += 1;
    }
}

fn assert_in_order(got: &[(Channel, Vec<u8>)]) {
    let mut expect = 0u32;
    for (channel, bytes) in got {
        if *channel == Channel::GAME_RPC {
            let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            assert_eq!(n, expect, "reliable messages arrive once, in order");
            expect += 1;
        }
    }
}

/// Row 11: a 300 ms receiver hitch (a shader compile) while a join snapshot streams must not put
/// the sender into outage recovery or throttle the stream afterwards.
#[test]
fn row11_a_300ms_receiver_hitch_does_not_throttle_a_bulk_stream() {
    let mut r = Rig::new(delayed(40));
    let mut next = 0u32;
    let mut at = HashMap::new();
    for now in (0..6_000u64).step_by(10) {
        feed_bulk(&mut r.a, &mut next);
        r.b_stalled = (2_000..2_300).contains(&now);
        r.step(now);
        at.insert(now, r.reliable_got());
    }
    assert_in_order(&r.got);
    let before = at[&1_990] - at[&990];
    let after = at[&3_490] - at[&2_490];
    let stats = r.a.stats();
    println!(
        "ROW11 hitch300 before={before}/s after={after}/s recoveries={} resends={} lost={}",
        stats.recoveries, stats.reliable_resends, stats.packets_lost
    );
    assert_eq!(
        stats.recoveries, 0,
        "a hitch shorter than an outage is not a recovery"
    );
    assert!(
        after * 10 >= before * 8,
        "the stream keeps its rate after a hitch: {before}/s before, {after}/s after"
    );
}

/// Row 11: a long stall with no loss (a 3 s level load on the receiver, whose socket kept the
/// datagrams) may count as an outage, but recovery must scale from what the path delivered before
/// it instead of a fixed 32 packets per second.
#[test]
fn row11_a_long_stall_without_loss_resumes_near_the_previous_rate() {
    let mut r = Rig::new(delayed(40));
    let mut next = 0u32;
    let mut at = HashMap::new();
    for now in (0..9_000u64).step_by(10) {
        feed_bulk(&mut r.a, &mut next);
        r.b_stalled = (2_000..5_000).contains(&now);
        r.step(now);
        at.insert(now, r.reliable_got());
    }
    assert_in_order(&r.got);
    let before = at[&1_990] - at[&990];
    let after = at[&6_490] - at[&5_490];
    let stats = r.a.stats();
    println!(
        "ROW11 stall3000 before={before}/s after={after}/s recoveries={} resends={}",
        stats.recoveries, stats.reliable_resends
    );
    assert!(
        after * 2 >= before,
        "recovery scales from the earlier delivery rate: {before}/s before, {after}/s after"
    );
}

/// Row 11: good-wifi under a reliable load never enters recovery and delivers everything once,
/// in order; the loss it counts and the data it repeats are what the link dropped (the base
/// counted 98,391 packets lost for 510 dropped).
#[test]
fn row11_good_wifi_under_reliable_load_never_recovers() {
    let wifi = ConditionerConfig {
        enabled: true,
        delay_ms: 4,
        jitter_ms: 4,
        loss: 0.002,
        reorder: 0.001,
        reorder_max_ms: 10,
        duplicate: 0.0005,
        ..Default::default()
    };
    let mut r = Rig::new(wifi);
    let mut next = 0u32;
    for now in (0..60_000u64).step_by(10) {
        if now < 55_000 {
            feed_bulk(&mut r.a, &mut next);
        }
        r.step(now);
    }
    assert_in_order(&r.got);
    let stats = r.a.stats();
    println!(
        "ROW11 good-wifi delivered={} recoveries={} resends={} lost={} injected={}",
        r.reliable_got(),
        stats.recoveries,
        stats.reliable_resends,
        stats.packets_lost,
        r.down.stats().dropped
    );
    assert_eq!(r.reliable_got() as u32, next, "everything queued was delivered");
    assert_eq!(stats.recoveries, 0);
    // 6,400 packets a second through jitter that shuffles every burst: what is counted lost
    // and what is repeated is what the link dropped, not what it reordered.
    let injected = r.down.stats().dropped;
    assert!(
        stats.packets_lost <= injected + 5 && stats.packets_lost + 5 >= injected,
        "measured loss {} against {injected} injected",
        stats.packets_lost
    );
    assert!(
        stats.reliable_resends <= injected + 16,
        "{} resends for {injected} lost packets",
        stats.reliable_resends
    );
}

/// Row 15: while a connection paces an outage backlog it still acknowledges a packet that carried
/// data promptly (the peer's round-trip estimate must not absorb the keepalive interval); an
/// acknowledgment-only packet is never echoed. Wire 5 (ADR-8184): "promptly" is within
/// `MAX_ACK_DELAY_MS`, a delay the acknowledgment reports and the peer subtracts.
#[test]
fn row15_recovery_acknowledges_data_at_once_and_coalesces_empty_echoes() {
    use regia_net::connection::MAX_ACK_DELAY_MS;
    let mut a = Connection::new(LinkConfig::default());
    // `b` sends no keepalives of its own, so its replies are acknowledgment-only packets.
    let mut b = Connection::new(LinkConfig {
        keepalive_ms: 1_000_000,
        ..LinkConfig::default()
    });
    // One exchange first: an outage is only judged on a path that has been measured.
    for p in a.poll(0, &[]) {
        b.receive(&p, 0).expect("hello");
    }
    for p in b.poll(0, &[]) {
        a.receive(&p, 0).expect("ack");
    }
    a.send(Channel::GAME_RPC, vec![7; 1100], 0).expect("queue");
    // Nothing reaches `b` for a second: `a` is recovering.
    let mut last = Vec::new();
    for now in (0..=1_000u64).step_by(10) {
        if let Some(p) = a.poll(now, &[]).pop() {
            last = p;
        }
    }
    assert!(
        a.stats().recovering,
        "a second of silence with reliable data waiting"
    );
    // `b` acknowledges a's latest packet (a keepalive ping) with an acknowledgment-only packet.
    let acks_before = b.stats().ack_only_sent;
    b.receive(&last, 1_030).expect("ping");
    let empty = b.poll(1_030, &[]).remove(0);
    assert_eq!(b.stats().ack_only_sent, acks_before + 1, "acknowledgment only");
    let echoes = a.stats().ack_only_sent;
    a.receive(&empty, 1_030).expect("empty");
    a.poll(1_030, &[]);
    assert_eq!(
        a.stats().ack_only_sent,
        echoes,
        "an acknowledgment is never echoed"
    );
    // A packet that carries data is acknowledged within the ack delay, which the frame reports.
    b.send(Channel::Unreliable, vec![1, 2, 3], 0).expect("queue");
    let data = b.poll(1_040, &[]).remove(0);
    let pn = u64::from(u16::from_le_bytes([data[1], data[2]]));
    a.receive(&data, 1_040).expect("data");
    let mut acked = None;
    for now in 1_040..=1_040 + MAX_ACK_DELAY_MS {
        for p in a.poll(now, &[]) {
            if let Some(frame) = ack_frame(&p)
                && frame.contains(pn)
                && acked.is_none()
            {
                acked = Some((now, frame.delay_us));
            }
        }
    }
    let (at, delay_us) = acked.expect("data is acknowledged within the ack delay during recovery");
    assert_eq!(delay_us, (at - 1_040) * 1_000, "the frame reports the hold");
}

/// Row 2 with acknowledgment trains: a burst larger than one acknowledgment can name is still
/// acknowledged in full by the next reply, so nothing is resent or counted lost.
#[test]
fn row2_a_64_packet_burst_is_acknowledged_in_full() {
    let mut a = Connection::new(blind_link());
    let mut b = Connection::new(blind_link());
    for id in 0..64u8 {
        a.send(Channel::GAME_RPC, vec![id; 1100], 0).expect("queue");
    }
    let burst = a.poll(0, &[]);
    assert_eq!(burst.len(), 64);
    for p in &burst {
        b.receive(p, 20).expect("data");
    }
    for p in b.poll(20, &[]) {
        a.receive(&p, 40).expect("ack");
    }
    // Later polls must have nothing to repeat.
    for now in (50..2_000u64).step_by(10) {
        for p in a.poll(now, &[]) {
            b.receive(&p, now).expect("packet");
        }
        for p in b.poll(now, &[]) {
            a.receive(&p, now).expect("ack");
        }
    }
    let stats = a.stats();
    println!(
        "ROW2 burst64 acked={} lost={} resends={}",
        stats.packets_acked, stats.packets_lost, stats.reliable_resends
    );
    assert_eq!(stats.reliable_in_flight, 0);
    assert_eq!(stats.reliable_resends, 0, "a delivered burst is never resent");
    assert_eq!(stats.packets_lost, 0, "a delivered burst is never counted lost");
}

/// Row 2: a reliable bulk transfer on a clean 100 ms path runs far above the 3 Mbit/s the
/// 32-packet horizon allowed, with no spurious resend once the round trip is known.
#[test]
fn row2_bulk_reliable_at_100ms_rtt_is_not_horizon_limited() {
    let mut r = Rig::new(delayed(100));
    // Keepalives measure the path first: before the first sample the timer is the configured
    // 100 ms minimum, which this path's first round trip would outlast.
    for tick in 0..60u64 {
        r.step(tick * 1000 / 60);
    }
    let mut next = 0u32;
    for tick in 60..660u64 {
        feed_bulk(&mut r.a, &mut next);
        r.step(tick * 1000 / 60);
    }
    assert_in_order(&r.got);
    let stats = r.a.stats();
    let bytes_per_s = r.reliable_got() * 1000 / 10;
    println!(
        "ROW2 bulk100 delivered={} bytes_per_s={bytes_per_s} resends={} lost={} rtt_ms={:.0}",
        r.reliable_got(),
        stats.reliable_resends,
        stats.packets_lost,
        stats.rtt_ms
    );
    assert!(bytes_per_s >= 1_000_000, "at least 8 Mbit/s: {bytes_per_s} B/s");
    assert_eq!(stats.reliable_resends, 0, "nothing delivered is resent");
    assert_eq!(stats.packets_lost, 0);
}

/// Before the first round-trip sample the resend timer is the configured minimum. On a path
/// slower than that, the first round trip repeats at most a few packets per poll, never the
/// whole burst, and nothing after the first acknowledgment.
#[test]
fn row2_the_first_round_trip_repeats_at_most_a_few_packets() {
    let mut r = Rig::new(delayed(100));
    let mut next = 0u32;
    for tick in 0..300u64 {
        feed_bulk(&mut r.a, &mut next);
        r.step(tick * 1000 / 60);
    }
    assert_in_order(&r.got);
    let stats = r.a.stats();
    println!(
        "ROW2 bulk100-cold delivered={} resends={} lost={}",
        r.reliable_got(),
        stats.reliable_resends,
        stats.packets_lost
    );
    assert!(
        stats.reliable_resends <= 16,
        "bounded repeats before the path is known: {}",
        stats.reliable_resends
    );
    assert_eq!(stats.packets_lost, 0);
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
    let stats = r.a.stats();
    println!(
        "ROW2 rate rtt={rtt} offered={offered} delivered={} lost={} rtt_ms={:.0}",
        r.got.len(),
        stats.packets_lost,
        stats.rtt_ms
    );
    assert_eq!(
        r.got.len(),
        offered,
        "180 packets/s are not limited per round trip"
    );
    assert_eq!(stats.packets_lost, 0, "a delivered packet is never counted lost");
}

/// Row 2: a 60 Hz sender with three state frames per tick keeps its rate at 250 ms.
#[test]
fn row2_rate_180pps_at_250ms_rtt() {
    rate_case(250);
}

/// Rows 2 and 4: and at the rtt-extreme profile's 3 s round trip, where no packet may be counted
/// lost just because its acknowledgment takes longer than a fixed timeout.
#[test]
fn row2_rate_180pps_at_3000ms_rtt() {
    rate_case(3000);
}

/// Row 3: reordering alone is never loss (spec 120 § 4.1 item 4 and 7).
#[test]
fn row3_reorder_only_is_never_loss() {
    for (reorder, reorder_max_ms) in [(0.15, 40), (0.30, 120)] {
        let mut r = Rig::new(ConditionerConfig {
            reorder,
            reorder_max_ms,
            ..delayed(250)
        });
        for tick in 0..1_320u64 {
            if tick < 1_200 {
                r.a.send(Channel::Unreliable, vec![7; 100], 0).expect("queue");
            }
            r.step(tick * 1000 / 60);
        }
        let stats = r.a.stats();
        println!(
            "ROW3 reorder={reorder} max={reorder_max_ms} delivered={} lost={} fraction={}",
            r.got.len(),
            stats.packets_lost,
            stats.loss_fraction
        );
        assert_eq!(stats.packets_lost, 0, "delivered reorder is not loss");
        assert_eq!(stats.loss_fraction, 0.0);
    }
}

/// Row 3: under the built-in `wan` and `wan_stress` profiles the measured loss is the injected
/// loss: reorder does not inflate it and real loss is still found.
#[test]
fn row3_measured_loss_tracks_injected_loss() {
    for profile in ["wan", "wan_stress"] {
        let mut r = Rig::new(ConditionerConfig::from_profile(profile).expect("profile"));
        for tick in 0..3_120u64 {
            if tick < 3_000 {
                r.a.send(Channel::Unreliable, vec![7; 100], 0).expect("queue");
            }
            r.step(tick * 1000 / 60);
        }
        let stats = r.a.stats();
        let injected = r.down.stats().dropped;
        println!(
            "ROW3 profile={profile} sent={} lost={} injected={injected} fraction={}",
            stats.packets_sent, stats.packets_lost, stats.loss_fraction
        );
        assert!(
            stats.packets_lost <= injected + 2,
            "{profile}: reorder must not inflate loss ({} counted, {injected} injected)",
            stats.packets_lost
        );
        assert!(
            stats.packets_lost + 2 >= injected,
            "{profile}: real loss must be found ({} counted, {injected} injected)",
            stats.packets_lost
        );
    }
}

/// Row 4: a 2.5 s link measures its round trip and counts nothing lost.
#[test]
fn row4_a_2500ms_link_measures_its_rtt_without_loss() {
    let mut r = Rig::new(delayed(2500));
    for tick in 0..1_000 {
        r.step(tick * 20);
    }
    let stats = r.a.stats();
    println!("ROW4 rtt={} lost={}", stats.rtt_ms, stats.packets_lost);
    assert!((2400.0..2700.0).contains(&stats.rtt_ms), "rtt {}", stats.rtt_ms);
    assert_eq!(stats.packets_lost, 0);
}

/// Row 4: reliable data on a 3 s round trip is not resent before its acknowledgment can arrive.
#[test]
fn row4_reliable_data_at_3000ms_rtt_is_not_resent_once_the_rtt_is_known() {
    let mut r = Rig::new(delayed(3000));
    // Let the estimator see the path first.
    for tick in 0..500u64 {
        r.step(tick * 20);
    }
    let before = r.a.stats().reliable_resends;
    for i in 0..200u32 {
        let mut m = vec![0u8; 1000];
        m[..4].copy_from_slice(&i.to_le_bytes());
        r.a.send(Channel::GAME_RPC, m, 0).expect("queue");
    }
    for tick in 500..1_500u64 {
        r.step(tick * 20);
    }
    assert_in_order(&r.got);
    let stats = r.a.stats();
    println!(
        "ROW4 reliable3000 delivered={} resends={} lost={}",
        r.reliable_got(),
        stats.reliable_resends - before,
        stats.packets_lost
    );
    assert_eq!(r.reliable_got(), 200);
    assert_eq!(
        stats.reliable_resends - before,
        0,
        "the resend timer never undercuts the measured round trip"
    );
}

/// Row 6: an authenticated peer that acknowledges only the newest packet with an empty mask
/// cannot make a fragment repeat more than once per round trip, nor more than a few per poll.
#[test]
fn row6_hostile_zero_masks_bound_the_resends() {
    let mut r = Rig::new(delayed(250));
    for now in (0..2000).step_by(5) {
        r.step(now);
    }
    let rtt = r.a.stats().rtt_ms.floor() as u64;
    assert!(rtt >= 250);
    for _ in 0..256 {
        r.a.send(Channel::GAME_RPC, vec![9; 1186], 0).expect("queue");
    }
    let mut ack_seq = 400u16;
    let mut last = HashMap::new();
    let mut min_spacing = u64::MAX;
    let mut max_resends = 0;
    let start_bytes = r.a.stats().bytes_sent;
    let mut in_window: VecDeque<(u64, u64)> = VecDeque::new();
    let mut peak_bytes_per_s = 0u64;
    for now in (2000..7000).step_by(5) {
        let mut resends = 0;
        for p in r.a.poll(now, &[]) {
            if let Some(id) = first_reliable_id(&p)
                && let Some(previous) = last.insert(id, now)
            {
                min_spacing = min_spacing.min(now - previous);
                resends += 1;
            }
            in_window.push_back((now, p.len() as u64));
            r.down.process(p, addr(), now);
        }
        while in_window.front().is_some_and(|(t, _)| now - t >= 1_000) {
            in_window.pop_front();
        }
        if now >= 4_000 {
            peak_bytes_per_s = peak_bytes_per_s.max(in_window.iter().map(|(_, n)| n).sum());
        }
        max_resends = max_resends.max(resends);
        let delivered = r.down.release_due(now);
        if let Some((newest, _)) = delivered.last() {
            // Reports only the largest packet (wire 5: a frame with one one-packet range).
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
            r.up.process(w.finish(), addr(), now);
        }
        for (p, _) in r.up.release_due(now) {
            r.a.receive(&p, now).expect("ack");
        }
    }
    let bytes = r.a.stats().bytes_sent - start_bytes;
    println!(
        "ROW6 hostile bytes={bytes} resends={} min_spacing={min_spacing} max_per_poll={max_resends} rtt={rtt} late_peak_bytes_per_s={peak_bytes_per_s}",
        r.a.stats().reliable_resends
    );
    assert!(
        min_spacing >= rtt,
        "a fragment repeats at most once per round trip"
    );
    assert!(max_resends <= 8, "resends are bounded per poll");
    assert!(bytes <= 256 * 1200 * (1 + 5000 / rtt));
    assert!(
        peak_bytes_per_s <= 64 * 1200 * 4,
        "a peer that acknowledges almost nothing is held to a small window: {peak_bytes_per_s} B/s"
    );
}

/// A lost burst followed by silence is still repaired by probes, and the stream ends complete
/// (tail loss: nothing newer is acknowledged, so only the timer can find it).
#[test]
fn tail_loss_is_repaired_by_the_probe_timer() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    for id in 0..20u8 {
        a.send(Channel::GAME_RPC, vec![id; 1100], 0).expect("queue");
    }
    // The last ten packets of the burst are lost.
    for (i, p) in a.poll(0, &[]).into_iter().enumerate() {
        if i < 10 {
            b.receive(&p, 5).expect("data");
        }
    }
    let mut got = Vec::new();
    for now in (10..3_000u64).step_by(10) {
        for p in b.poll(now, &[]) {
            a.receive(&p, now).expect("ack");
        }
        for p in a.poll(now, &[]) {
            b.receive(&p, now).expect("data");
        }
        got.extend(std::iter::from_fn(|| b.next_message()).map(|(_, m)| m[0]));
    }
    assert_eq!(got, (0..20).collect::<Vec<u8>>());
    assert_eq!(a.stats().reliable_in_flight, 0);
    assert!(
        a.stats().reliable_resends <= 12,
        "only the lost tail is repeated: {}",
        a.stats().reliable_resends
    );
}
