// SPDX-License-Identifier: MPL-2.0
//! Per-peer inbound policing on the server (spec 120 § 11.2, § 11.3; ADR-8181).
//!
//! Three things keep one connected peer from lagging the others or hiding hostile traffic:
//!
//! - **Rate buckets.** Every connected peer has a token bucket on packets, one on bytes and one on
//!   messages. The packet and byte buckets are charged *before* the AEAD open, right after the
//!   cheap address-and-session-token lookup, so a datagram over them costs no cipher work. The
//!   message bucket is charged per message after the open. Over a bucket the datagram (or the
//!   unreliable message) is dropped and the peer is marked over rate for this receive.
//! - **Sustained-flood timer.** Each receive interval in which the peer was over rate adds the
//!   interval's length to a flood timer; an interval without it takes half its length off. A
//!   peer whose timer reaches [`InboundRateConfig::flood_disconnect_ms`] (2 s) is removed with
//!   `DisconnectReason::Flood`. A continuous flood is dropped after 2 s; a duty-cycled one still
//!   climbs while it floods more than a third of the time. An interval longer than one second
//!   (a server stall) does not count either way.
//! - **Malformed leak.** The malformed score is a leaky bucket in time: each refused message adds
//!   its kind's weight (`RejectKind::malformed_weight`: 16 points for an authenticated message that
//!   failed to decode, ADR-8606), the score leaks [`MALFORMED_LEAK_PER_S`] points a second, and good
//!   or empty packets earn nothing. Bad/good alternation therefore climbs to the limit instead of
//!   cancelling out.
//! - **Attribution (ADR-8606).** Only what authenticated as the peer's can count against it. A
//!   sealed record from the peer's address that fails its open (forged, replayed, damaged) gets back
//!   the packet and byte tokens and the slice place the rate check took, so a spoofer cannot spend
//!   an honest peer's rate or feed its flood timer. Each datagram is opened on its own merits: no
//!   count of failed opens stops a later one from being opened, so a stranger who copied the
//!   peer's cleartext token cannot hide the peer's own datagrams, good or bad, behind a front of
//!   forgeries (ADR-8666, replacing ADR-8606's per-receive cap). The opens one receive does are
//!   bounded by the server's read cap per receive instead.
//!
//! The bucket depth is the configured burst plus the bucket's rate over the time since the last
//! receive (at least one tick, at most one second), so a server that receives once a tick, or
//! after a short stall, still admits everything an honest peer sent in between.
//!
//! Token buckets are the policing form of RFC 2697 / RFC 2698 (single and two rate markers);
//! RFC 8085 § 3.1 asks a UDP receiver to bound what one sender may make it do. The code is original.

use serde::{Deserialize, Serialize};

/// Default [`InboundRateConfig::burst_packets`]: 64 datagrams (spec 120 § 11.3), one full
/// `LinkConfig::max_packets_per_poll` of the default link.
pub const DEFAULT_BURST_PACKETS: u32 = 64;
/// Default [`InboundRateConfig::flood_disconnect_ms`]: two seconds over rate (spec 120 § 11.3).
pub const DEFAULT_FLOOD_DISCONNECT_MS: u64 = 2_000;
/// The derived packet and byte rates are this many times a peer's send allowance.
pub const ALLOWANCE_MULTIPLE: u64 = 4;
/// Malformed points a peer's score leaks per second (ADR-8181). With the limit of 1,024 points and
/// 16 points per authenticated undecodable message (ADR-8606), a peer sending fewer than one every
/// 4 s never climbs, 63 a second is removed in about 1 s, and 5 a second in about 13.5 s.
pub const MALFORMED_LEAK_PER_S: u64 = 4;
/// A receive interval longer than this (a server stall) neither feeds nor drains the flood timer,
/// and the bucket depth stops growing at it.
pub const MAX_COUNTED_INTERVAL_MS: u64 = 1_000;
/// Fixed-point scale of the buckets: tokens are kept in thousandths so refills are exact integers.
const SCALE: u64 = 1_000;

/// Per-peer inbound rate policy of a [`crate::server::NetServer`] (spec 120 § 11.3). A `0` rate is
/// derived from the server's tick rate and link configuration ([`InboundRateConfig::resolve`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InboundRateConfig {
    /// Police connected peers at all. Off, only the per-receive message caps remain (the old
    /// behavior); keep it on for any server reachable by untrusted clients.
    pub enabled: bool,
    /// Datagrams per second one peer may send (`0` = 4 x `max_packets_per_poll` x the tick rate).
    pub packets_per_s: u32,
    /// Datagram bytes per second one peer may send (`0` = the packet rate x `max_packet_bytes`).
    pub bytes_per_s: u64,
    /// Messages per second one peer may deliver (`0` = 4,096 x the tick rate, the old per-receive
    /// cap at one receive per tick). Over it, unreliable messages are dropped; reliable ones are
    /// always delivered (they cannot be dropped) but still mark the peer over rate.
    pub messages_per_s: u32,
    /// Datagrams a peer may send at once above its rate.
    pub burst_packets: u32,
    /// Milliseconds over rate, net of recovery, after which the peer is removed with
    /// `DisconnectReason::Flood`.
    pub flood_disconnect_ms: u64,
}

impl Default for InboundRateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            packets_per_s: 0,
            bytes_per_s: 0,
            messages_per_s: 0,
            burst_packets: DEFAULT_BURST_PACKETS,
            flood_disconnect_ms: DEFAULT_FLOOD_DISCONNECT_MS,
        }
    }
}

/// The rates in force after the derived defaults were filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundRates {
    /// Datagrams per second.
    pub packets_per_s: u64,
    /// Datagram bytes per second.
    pub bytes_per_s: u64,
    /// Messages per second.
    pub messages_per_s: u64,
    /// Burst in datagrams.
    pub burst_packets: u64,
    /// Burst in bytes (`burst_packets` full datagrams).
    pub burst_bytes: u64,
    /// Burst in messages (one per-receive message cap).
    pub burst_messages: u64,
    /// One tick, in milliseconds: the least interval the bucket depth covers.
    pub tick_ms: u64,
}

impl InboundRateConfig {
    /// The rates in force for a server ticking at `tick_rate_hz` whose peers send packets of at
    /// most `max_packet_bytes`, `max_packets_per_poll` per poll, and deliver at most
    /// `messages_per_receive` messages per receive.
    pub fn resolve(
        &self,
        tick_rate_hz: u32,
        max_packet_bytes: usize,
        max_packets_per_poll: usize,
        messages_per_receive: u32,
    ) -> InboundRates {
        let tick = u64::from(tick_rate_hz.max(1));
        let packet_bytes = (max_packet_bytes as u64).max(1);
        let packets_per_s = if self.packets_per_s > 0 {
            u64::from(self.packets_per_s)
        } else {
            ALLOWANCE_MULTIPLE * (max_packets_per_poll as u64).max(1) * tick
        };
        let bytes_per_s = if self.bytes_per_s > 0 {
            self.bytes_per_s
        } else {
            packets_per_s.saturating_mul(packet_bytes)
        };
        let messages_per_s = if self.messages_per_s > 0 {
            u64::from(self.messages_per_s)
        } else {
            u64::from(messages_per_receive.max(1)) * tick
        };
        let burst_packets = u64::from(self.burst_packets.max(1));
        InboundRates {
            packets_per_s,
            bytes_per_s,
            messages_per_s,
            burst_packets,
            burst_bytes: burst_packets.saturating_mul(packet_bytes),
            burst_messages: u64::from(messages_per_receive.max(1)),
            tick_ms: (1_000 / tick).max(1),
        }
    }
}

/// One token bucket in thousandths of a token.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Tokens times [`SCALE`].
    milli: u64,
    /// Tokens per second.
    rate: u64,
    /// Tokens above the rate the bucket holds.
    burst: u64,
}

impl Bucket {
    fn full(rate: u64, burst: u64, tick_ms: u64) -> Self {
        let mut b = Self {
            milli: 0,
            rate,
            burst,
        };
        b.milli = b.depth_milli(tick_ms);
        b
    }

    /// The depth for an interval of `interval_ms` (clamped to one tick .. one second).
    fn depth_milli(&self, interval_ms: u64) -> u64 {
        let covered = self.rate.saturating_mul(interval_ms);
        self.burst.saturating_mul(SCALE).saturating_add(covered)
    }

    /// Adds `elapsed_ms` of rate, capped at the depth for that interval (one tick .. one second).
    /// A bucket already above the cap keeps its tokens: a short refill right after a long one does
    /// not take back what the long one granted.
    fn refill(&mut self, elapsed_ms: u64, tick_ms: u64) {
        if elapsed_ms == 0 {
            return;
        }
        let interval = elapsed_ms.clamp(tick_ms, MAX_COUNTED_INTERVAL_MS);
        let gained = self.rate.saturating_mul(elapsed_ms);
        let capped = self.milli.saturating_add(gained).min(self.depth_milli(interval));
        self.milli = self.milli.max(capped);
    }

    /// Takes `n` tokens; false (and nothing taken) when fewer are left.
    fn take(&mut self, n: u64) -> bool {
        let cost = n.saturating_mul(SCALE);
        if self.milli >= cost {
            self.milli -= cost;
            true
        } else {
            false
        }
    }
}

/// One connected peer's inbound policing state, for hosts and `net.peers` (ADR-8181).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInboundStatus {
    /// Datagrams and unreliable messages dropped for the peer's rate or slice since it joined.
    pub over_rate_drops: u64,
    /// Milliseconds over rate, net of recovery; the peer is removed at
    /// [`InboundRateConfig::flood_disconnect_ms`].
    pub flood_ms: u64,
    /// Malformed score in points (it leaks [`MALFORMED_LEAK_PER_S`] a second); the peer is removed
    /// at `regia_net::server::MALFORMED_SCORE_LIMIT`.
    pub malformed_points: u64,
}

/// What the packet check decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PacketVerdict {
    /// Open and deliver it.
    Admit,
    /// Over the packet or byte bucket, or the peer's slice of this receive: dropped unopened.
    OverRate,
}

/// One peer's inbound state on the server.
#[derive(Debug, Clone)]
pub(crate) struct PeerInbound {
    packets: Bucket,
    bytes: Bucket,
    messages: Bucket,
    /// Datagrams of this peer this receive handled (admitted or not), for the per-receive slice.
    pub(crate) this_receive: u32,
    /// The server clock the buckets and the malformed score were brought up to.
    clock_ms: u64,
    /// One tick in milliseconds: the least interval a bucket's depth covers.
    tick_ms: u64,
    /// The peer went over a bucket or its slice during this receive.
    over: bool,
    /// Milliseconds over rate, net of recovery.
    flood_ms: u64,
    /// When the last receive that counted for the flood timer ended (server clock).
    last_ms: u64,
    /// Datagrams and messages dropped for this peer's rate, ever.
    pub(crate) dropped: u64,
    /// Malformed points times [`SCALE`].
    malformed_milli: u64,
}

impl PeerInbound {
    pub(crate) fn new(rates: &InboundRates, now_ms: u64) -> Self {
        Self {
            packets: Bucket::full(rates.packets_per_s, rates.burst_packets, rates.tick_ms),
            bytes: Bucket::full(rates.bytes_per_s, rates.burst_bytes, rates.tick_ms),
            messages: Bucket::full(rates.messages_per_s, rates.burst_messages, rates.tick_ms),
            this_receive: 0,
            clock_ms: now_ms,
            tick_ms: rates.tick_ms,
            over: false,
            flood_ms: 0,
            last_ms: now_ms,
            dropped: 0,
            malformed_milli: 0,
        }
    }

    /// Brings the buckets and the malformed score up to `now_ms` (refill and leak). Every charge
    /// calls it, so a host or test that hands datagrams over without `receive` still refills.
    pub(crate) fn advance(&mut self, now_ms: u64) {
        let elapsed = now_ms.saturating_sub(self.clock_ms);
        if elapsed == 0 {
            return;
        }
        self.clock_ms = now_ms;
        // A receive reads at one instant, so a later instant is a later receive for the slice.
        self.this_receive = 0;
        self.packets.refill(elapsed, self.tick_ms);
        self.bytes.refill(elapsed, self.tick_ms);
        self.messages.refill(elapsed, self.tick_ms);
        self.malformed_milli = self
            .malformed_milli
            .saturating_sub(MALFORMED_LEAK_PER_S.saturating_mul(elapsed));
    }

    /// Starts a receive at `now_ms`: brings the buckets up to it and resets the slice count.
    pub(crate) fn begin_receive(&mut self, now_ms: u64) {
        self.advance(now_ms);
        self.this_receive = 0;
    }

    /// Ends a receive at `now_ms`: feeds or drains the flood timer with the interval since the
    /// last receive. True when the timer reached `disconnect_ms`.
    pub(crate) fn end_receive(&mut self, now_ms: u64, disconnect_ms: u64) -> bool {
        let interval = now_ms.saturating_sub(self.last_ms);
        if interval == 0 {
            // Another receive at the same instant: the over flag waits for an interval to count in.
            return false;
        }
        self.last_ms = now_ms;
        let over = std::mem::take(&mut self.over);
        if interval > MAX_COUNTED_INTERVAL_MS {
            return false;
        }
        if over {
            self.flood_ms = self.flood_ms.saturating_add(interval);
        } else {
            self.flood_ms = self.flood_ms.saturating_sub(interval / 2);
        }
        self.flood_ms >= disconnect_ms
    }

    /// Charges one datagram of `len` bytes at `now_ms`, before any cipher work. `slice` is the most
    /// datagrams this peer may have handled in one receive.
    pub(crate) fn admit_packet(&mut self, now_ms: u64, len: usize, slice: u32) -> PacketVerdict {
        self.advance(now_ms);
        self.this_receive = self.this_receive.saturating_add(1);
        if self.this_receive > slice {
            return self.refuse();
        }
        // Take the byte tokens only when the packet token is there, so a refusal costs nothing.
        if self.packets.milli < SCALE || !self.bytes.take(len as u64) {
            return self.refuse();
        }
        self.packets.take(1);
        PacketVerdict::Admit
    }

    /// Gives back what [`Self::admit_packet`] took for an admitted datagram of `len` bytes whose
    /// open then failed (ADR-8606): it was not provably this peer's, so it must not spend the
    /// peer's rate or slice, and it does not stop a later datagram from being opened (ADR-8666).
    pub(crate) fn refund_packet(&mut self, len: usize) {
        self.this_receive = self.this_receive.saturating_sub(1);
        self.packets.milli = self.packets.milli.saturating_add(SCALE);
        self.bytes.milli = self
            .bytes
            .milli
            .saturating_add((len as u64).saturating_mul(SCALE));
    }

    fn refuse(&mut self) -> PacketVerdict {
        self.over = true;
        self.dropped += 1;
        PacketVerdict::OverRate
    }

    /// Charges one delivered message. False when the message bucket is empty: the peer is over
    /// rate (an unreliable message is then dropped by the caller and counted with
    /// [`Self::note_message_dropped`]).
    pub(crate) fn admit_message(&mut self, now_ms: u64) -> bool {
        self.advance(now_ms);
        if self.messages.take(1) {
            true
        } else {
            self.over = true;
            false
        }
    }

    /// Counts one message the caller dropped for rate.
    pub(crate) fn note_message_dropped(&mut self) {
        self.over = true;
        self.dropped += 1;
    }

    /// Adds `points` malformed points at `now_ms`; true when the score reached `limit`.
    pub(crate) fn charge_malformed(&mut self, now_ms: u64, points: u32, limit: u32) -> bool {
        self.advance(now_ms);
        self.malformed_milli = self
            .malformed_milli
            .saturating_add(u64::from(points).saturating_mul(SCALE));
        self.malformed_milli >= u64::from(limit).saturating_mul(SCALE)
    }

    /// The malformed score, in whole points.
    pub(crate) fn malformed_points(&self) -> u64 {
        self.malformed_milli / SCALE
    }

    /// Milliseconds over rate, net of recovery.
    pub(crate) fn flood_ms(&self) -> u64 {
        self.flood_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates() -> InboundRates {
        // A 20 Hz server with the default link: 4 x 64 x 20 = 5,120 packets/s.
        InboundRateConfig::default().resolve(20, 1200, 64, 4096)
    }

    #[test]
    fn derived_defaults_follow_the_tick_rate_and_link() {
        let r = rates();
        assert_eq!(r.packets_per_s, 5_120);
        assert_eq!(r.bytes_per_s, 5_120 * 1_200);
        assert_eq!(r.messages_per_s, 4_096 * 20);
        assert_eq!((r.burst_packets, r.burst_bytes, r.tick_ms), (64, 64 * 1_200, 50));
        let fixed = InboundRateConfig {
            packets_per_s: 100,
            bytes_per_s: 10_000,
            messages_per_s: 50,
            ..InboundRateConfig::default()
        }
        .resolve(20, 1200, 64, 4096);
        assert_eq!(
            (fixed.packets_per_s, fixed.bytes_per_s, fixed.messages_per_s),
            (100, 10_000, 50)
        );
    }

    #[test]
    fn a_peer_at_its_allowance_is_never_over() {
        // 64 packets per 50 ms tick for a minute: a quarter of the rate.
        let r = rates();
        let mut p = PeerInbound::new(&r, 0);
        for k in 1..=1_200u64 {
            p.begin_receive(k * 50);
            for _ in 0..64 {
                assert_eq!(p.admit_packet(0, 1_200, 4_096), PacketVerdict::Admit);
            }
            assert!(!p.end_receive(k * 50, 2_000));
        }
        assert_eq!(p.dropped, 0);
        assert_eq!(p.flood_ms(), 0);
    }

    #[test]
    fn a_continuous_flood_trips_the_timer_at_two_seconds() {
        // 100k/s at a 20 Hz receive: 5,000 datagrams per receive.
        let r = rates();
        let mut p = PeerInbound::new(&r, 0);
        let mut tripped = None;
        for k in 1..=100u64 {
            p.begin_receive(k * 50);
            let admitted = (0..5_000)
                .filter(|_| p.admit_packet(0, 100, 4_096) == PacketVerdict::Admit)
                .count();
            assert!(admitted <= 64 + 256 + 256, "receive {k} admitted {admitted}");
            if p.end_receive(k * 50, 2_000) {
                tripped = Some(k * 50);
                break;
            }
        }
        // The flood started before the first receive at 50 ms; 40 intervals of 50 ms later.
        assert_eq!(tripped, Some(2_000));
    }

    #[test]
    fn a_short_burst_recovers_and_a_duty_cycled_flood_still_trips() {
        let r = rates();
        let mut p = PeerInbound::new(&r, 0);
        // A 200 ms burst far over rate, then honest traffic: the timer drains back to zero.
        for k in 1..=100u64 {
            p.begin_receive(k * 50);
            let n = if (1..=4).contains(&k) { 5_000 } else { 3 };
            for _ in 0..n {
                p.admit_packet(0, 100, 4_096);
            }
            assert!(!p.end_receive(k * 50, 2_000), "burst tripped at receive {k}");
        }
        assert_eq!(p.flood_ms(), 0);
        // Flooding 1 s of every 1.5 s nets +750 ms a cycle: it trips.
        let mut p = PeerInbound::new(&r, 0);
        let tripped = (1..=1_000u64).any(|k| {
            p.begin_receive(k * 50);
            let n = if k % 30 < 20 { 5_000 } else { 3 };
            for _ in 0..n {
                p.admit_packet(0, 100, 4_096);
            }
            p.end_receive(k * 50, 2_000)
        });
        assert!(tripped);
    }

    #[test]
    fn the_slice_and_a_stall_bound_one_receive() {
        let r = rates();
        let mut p = PeerInbound::new(&r, 0);
        p.begin_receive(50);
        let admitted = (0..200)
            .filter(|_| p.admit_packet(0, 100, 128) == PacketVerdict::Admit)
            .count();
        assert_eq!(admitted, 128, "the slice caps one receive");
        assert!(!p.end_receive(50, 2_000));
        // After a 1 s stall an honest peer's whole second (its allowance, 1,280) is admitted, and
        // the stall interval does not feed the timer.
        p.begin_receive(1_050);
        let admitted = (0..1_280)
            .filter(|_| p.admit_packet(0, 1_200, 4_096) == PacketVerdict::Admit)
            .count();
        assert_eq!(admitted, 1_280);
        assert!(!p.end_receive(1_050, 2_000));
    }

    #[test]
    fn malformed_points_leak_with_time_not_with_good_packets() {
        let r = rates();
        let mut p = PeerInbound::new(&r, 0);
        // One bad message a second for ten minutes: the leak (4/s) keeps it at zero.
        for s in 1..=600u64 {
            p.begin_receive(s * 1_000);
            assert!(!p.charge_malformed(0, 1, 1_024));
            p.end_receive(s * 1_000, 2_000);
        }
        assert!(p.malformed_points() <= 1);
        // 30 bad a second (bad/good alternation at 60 Hz): 26 net a second, past 1,024 in 40 s.
        let mut p = PeerInbound::new(&r, 0);
        let mut at = None;
        'outer: for k in 1..=1_200u64 {
            p.begin_receive(k * 50);
            for _ in 0..(if k % 2 == 0 { 2 } else { 1 }) {
                if p.charge_malformed(0, 1, 1_024) {
                    at = Some(k * 50);
                    break 'outer;
                }
            }
            p.end_receive(k * 50, 2_000);
        }
        let at = at.expect("alternation reaches the limit");
        assert!((35_000..=45_000).contains(&at), "dropped at {at} ms");
    }

    /// ADR-8606: an admitted datagram whose open failed gives its tokens and slice place back, so
    /// any number of forged records under the peer's address and token (up to the failed-open cap)
    /// never put the peer over rate; past the cap they are dropped unattributed, never marking it.
    #[test]
    fn failed_opens_are_refunded_and_never_stop_a_later_open() {
        let r = rates();
        let mut p = PeerInbound::new(&r, 0);
        for k in 1..=200u64 {
            p.begin_receive(k * 50);
            // 400 forged records (8,000/s: over the 5,120/s rate) around 3 honest ones.
            for i in 0..403 {
                assert_eq!(
                    p.admit_packet(0, 1_200, 512),
                    PacketVerdict::Admit,
                    "receive {k} datagram {i}"
                );
                if i % 134 != 0 {
                    p.refund_packet(1_200);
                }
            }
            assert!(!p.end_receive(k * 50, 2_000));
        }
        assert_eq!((p.dropped, p.flood_ms()), (0, 0), "never over rate");
        // ADR-8666: past `slice` failed opens in one receive, the next datagram is still admitted
        // and opened (ADR-8606's cap dropped it unopened, so a forger holding the cleartext token
        // could hide the peer's own datagrams for the rest of the receive).
        let mut p = PeerInbound::new(&r, 0);
        p.begin_receive(50);
        for _ in 0..4 * 128 {
            assert_eq!(p.admit_packet(0, 100, 128), PacketVerdict::Admit);
            p.refund_packet(100);
        }
        assert_eq!(p.admit_packet(0, 100, 128), PacketVerdict::Admit);
        assert!(!p.end_receive(50, 2_000));
        assert_eq!((p.dropped, p.flood_ms()), (0, 0));
    }

    /// ADR-8606's arithmetic at the server's weights (16 points per refusal, the 4-point leak,
    /// 1,024 points): the soak's 63 a second goes in about 1 s, a drip of 5 a second in about
    /// 13.5 s, and one every 4 s never climbs past one refusal's points.
    #[test]
    fn weighted_refusals_meet_the_bars() {
        let r = rates();
        let first_drop = |every_ms: u64| {
            let mut p = PeerInbound::new(&r, 0);
            (1..=600_000 / every_ms).find_map(|k| {
                p.charge_malformed(k * every_ms, 16, 1_024)
                    .then_some(k * every_ms)
            })
        };
        // 63 a second: one every 16 ms.
        let soak = first_drop(16).expect("the soak rate is removed");
        assert!((1_000..=1_100).contains(&soak), "soak rate removed at {soak} ms");
        let drip = first_drop(200).expect("5 a second is removed");
        assert!((13_000..=14_000).contains(&drip), "drip removed at {drip} ms");
        assert_eq!(first_drop(4_000), None, "one every 4 s never accumulates");
        let mut p = PeerInbound::new(&r, 0);
        for k in 1..=150u64 {
            assert!(!p.charge_malformed(k * 4_000, 16, 1_024));
            assert!(p.malformed_points() <= 16);
        }
    }
}
