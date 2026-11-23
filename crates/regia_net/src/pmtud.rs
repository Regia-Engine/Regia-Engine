// SPDX-License-Identifier: MPL-2.0
//! Datagram packetization layer path MTU discovery for one connection's send direction (spec 120
//! § 4.3, RFC 8899, ADR-8300). Opt-in: [`crate::connection::LinkConfig::mtu_probing`].
//!
//! The connection starts at the base size, [`BASE_PLPMTU`] (1,200 bytes including every header and
//! the seal: RFC 9000 § 14, safe on IPv6's 1,280-byte minimum). Once the path has a round-trip
//! sample it sends padded probe packets of the next size in [`MTU_PROBE_SIZES`]. A probe is a ping
//! plus padding: it carries no data, so its loss costs nothing but the probe. Only an
//! acknowledgment of the probe raises the packet size; [`MAX_MTU_PROBES`] consecutive losses of one
//! size end the search at the size already confirmed (RFC 8899 § 5.2, `MAX_PROBES`).
//!
//! Black-hole detection (RFC 8899 § 4.3). A lost packet above the base size puts the size in
//! doubt: from the next packet on, data goes at the base size again and a confirmation probe of the
//! current size leaves at once. Its acknowledgment ends the doubt; [`MAX_MTU_PROBES`] consecutive
//! lost confirmations drop the size to the base for good (until the search restarts after
//! [`MTU_BLACK_HOLE_RETRY_MS`]). Independently, [`BLACK_HOLE_LOSS_BURST`] packets in a row above the
//! base size lost while a smaller packet sent after the first of them was acknowledged are a black
//! hole at once. A probe counts as lost only when a packet sent after it was acknowledged: during
//! an outage, when nothing gets through, an unanswered probe is inconclusive and is sent again.
//!
//! Reliable fragments are always cut for the base size, so after a fall back every unacknowledged
//! fragment still fits one packet: a black hole never strands reliable data. Only how many records
//! one packet carries, and the largest unreliable message, follow the probed size.
//!
//! The state machine here is pure (no clock, no sockets); [`crate::connection::Connection`] feeds
//! it sends, acknowledgments and losses.

use serde::{Deserialize, Serialize};

/// The base packet size: every path carries it, and probing never goes below it.
pub const BASE_PLPMTU: usize = 1200;
/// Probe sizes, smallest first, in bytes of the whole datagram (UDP payload): 1,280 (the IPv6
/// minimum link MTU as a payload), 1,400 (tunnels and VPNs), 1,452 (PPPoE: 1,492 minus the IPv4
/// and UDP headers, the most common Ethernet-class ceiling below 1,500).
pub const MTU_PROBE_SIZES: [usize; 3] = [1280, 1400, 1452];
/// Consecutive lost probes of one size that end the search or, for confirmation probes, mean a
/// black hole (RFC 8899 `MAX_PROBES`, default 3).
pub const MAX_MTU_PROBES: u8 = 3;
/// Larger packets lost in a row, with a smaller one acknowledged between them, that mean a black
/// hole.
pub const BLACK_HOLE_LOSS_BURST: u32 = 3;
/// After the search completes below the largest probe size, how long until it tries the next
/// size again (RFC 8899 `PMTU_RAISE_TIMER`, 600 s).
pub const MTU_RAISE_INTERVAL_MS: u64 = 600_000;
/// After a black hole, how long the connection stays at the base size before it searches again.
pub const MTU_BLACK_HOLE_RETRY_MS: u64 = 60_000;
/// With the size above the base and no packet of that size acknowledged for this long, a
/// confirmation probe checks the path still carries it (RFC 8899 `CONFIRMATION_TIMER`).
pub const MTU_CONFIRM_INTERVAL_MS: u64 = 5_000;
/// Smallest gap between two confirmation probes that a lost large packet asks for.
pub const MTU_CONFIRM_MIN_GAP_MS: u64 = 250;

/// Where the search stands (RFC 8899 § 5.2 states).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MtuSearch {
    /// Probing is off: the packet size is the configured one.
    #[default]
    Disabled,
    /// At the base size, waiting for a round-trip sample before the first probe.
    Base,
    /// Probing for the next size.
    Searching,
    /// The largest size the path carries is known (a raise is retried later).
    #[serde(rename = "search_complete")]
    Complete,
    /// A black hole dropped the size back to the base; the search restarts later.
    Error,
}

impl MtuSearch {
    /// Lowercase name for logs, status and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Base => "base",
            Self::Searching => "searching",
            Self::Complete => "search_complete",
            Self::Error => "error",
        }
    }
}

/// Path MTU counters of one connection (also in `LinkStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MtuCounters {
    /// Probe packets sent (search and confirmation).
    pub probes_sent: u64,
    /// Probe packets declared lost.
    pub probes_lost: u64,
    /// Black holes detected (each one dropped the size back to the base).
    pub black_holes: u64,
}

/// One probe in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outstanding {
    seq: u64,
    size: usize,
    deadline_ms: u64,
    confirm: bool,
}

/// The probe the connection should send now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeRequest {
    /// Datagram size of the probe.
    pub size: usize,
    /// It confirms the current size rather than raising it.
    pub confirm: bool,
}

/// DPLPMTUD state of one send direction. See the module docs.
#[derive(Debug, Clone)]
pub(crate) struct PathMtu {
    state: MtuSearch,
    base: usize,
    plpmtu: usize,
    /// Largest datagram the local transport accepts.
    ceiling: usize,
    /// Index of the next size to probe in [`MTU_PROBE_SIZES`].
    next_index: usize,
    /// Consecutive lost probes of the current target (search) or size (confirmation).
    lost_in_row: u8,
    outstanding: Option<Outstanding>,
    /// The next probe may leave from this time.
    next_probe_ms: u64,
    /// After a complete search, when the next size is tried again.
    raise_at_ms: u64,
    /// A packet above the base size was lost: data goes at the base size until a confirmation
    /// probe is acknowledged.
    doubt: bool,
    /// When a packet above the base size was last acknowledged.
    last_large_ack_ms: u64,
    /// Larger-than-base packets lost in a row, and the number of the first of them.
    loss_run: u32,
    loss_run_first: u64,
    /// A smaller packet sent between two losses of the run was acknowledged.
    loss_run_alive: bool,
    counters: MtuCounters,
}

impl PathMtu {
    /// A search over `base`'s send direction; `enabled` false keeps `base` for good.
    pub(crate) fn new(enabled: bool, base: usize) -> Self {
        Self {
            state: if enabled {
                MtuSearch::Base
            } else {
                MtuSearch::Disabled
            },
            base,
            plpmtu: base,
            ceiling: crate::socket::MAX_DATAGRAM_BYTES,
            next_index: MTU_PROBE_SIZES
                .iter()
                .position(|s| *s > base)
                .unwrap_or(MTU_PROBE_SIZES.len()),
            lost_in_row: 0,
            outstanding: None,
            next_probe_ms: 0,
            raise_at_ms: 0,
            doubt: false,
            last_large_ack_ms: 0,
            loss_run: 0,
            loss_run_first: 0,
            loss_run_alive: false,
            counters: MtuCounters::default(),
        }
    }

    /// True when probing is on.
    pub(crate) fn enabled(&self) -> bool {
        self.state != MtuSearch::Disabled
    }

    /// The confirmed packet size (by an acknowledgment, or the base).
    pub(crate) fn plpmtu(&self) -> usize {
        self.plpmtu.min(self.ceiling).max(self.base.min(self.ceiling))
    }

    /// The size data packets may use now: the confirmed size, or the base while it is in doubt.
    pub(crate) fn packet_size(&self) -> usize {
        if self.doubt {
            self.base.min(self.ceiling)
        } else {
            self.plpmtu()
        }
    }

    /// True while a lost large packet has put the size in doubt.
    #[cfg(test)]
    pub(crate) fn in_doubt(&self) -> bool {
        self.doubt
    }

    /// Search state.
    pub(crate) fn state(&self) -> MtuSearch {
        self.state
    }

    /// Counters.
    pub(crate) fn counters(&self) -> MtuCounters {
        self.counters
    }

    /// The base shrank (the transport's budget fell under it): the search starts over below it.
    pub(crate) fn set_base(&mut self, base: usize) {
        if base >= self.base {
            return;
        }
        self.base = base;
        self.plpmtu = self.plpmtu.min(base);
    }

    /// The local transport accepts datagrams of at most `max` bytes.
    pub(crate) fn set_ceiling(&mut self, max: usize) {
        self.ceiling = max.max(1);
    }

    /// The probe due now, if any. `path_known`: the connection has a round-trip sample.
    pub(crate) fn probe_due(&mut self, now_ms: u64, path_known: bool) -> Option<ProbeRequest> {
        if !self.enabled() || self.outstanding.is_some() || !path_known || now_ms < self.next_probe_ms {
            return None;
        }
        if self.doubt && self.plpmtu() > self.base {
            return Some(ProbeRequest {
                size: self.plpmtu(),
                confirm: true,
            });
        }
        match self.state {
            MtuSearch::Disabled => None,
            MtuSearch::Base | MtuSearch::Searching => self.search_target(now_ms),
            MtuSearch::Complete => {
                let stale = now_ms.saturating_sub(self.last_large_ack_ms) >= MTU_CONFIRM_INTERVAL_MS;
                if self.plpmtu() > self.base && stale {
                    return Some(ProbeRequest {
                        size: self.plpmtu(),
                        confirm: true,
                    });
                }
                // The raise timer ran out: try the next size again.
                if self.next_index < MTU_PROBE_SIZES.len() && now_ms >= self.raise_at_ms {
                    self.state = MtuSearch::Searching;
                    self.lost_in_row = 0;
                    return self.search_target(now_ms);
                }
                None
            }
            MtuSearch::Error => {
                self.state = MtuSearch::Searching;
                self.next_index = MTU_PROBE_SIZES
                    .iter()
                    .position(|s| *s > self.base)
                    .unwrap_or(MTU_PROBE_SIZES.len());
                self.lost_in_row = 0;
                self.search_target(now_ms)
            }
        }
    }

    fn search_target(&mut self, now_ms: u64) -> Option<ProbeRequest> {
        match MTU_PROBE_SIZES.get(self.next_index) {
            Some(&size) if size <= self.ceiling => {
                self.state = MtuSearch::Searching;
                Some(ProbeRequest { size, confirm: false })
            }
            _ => {
                self.complete(now_ms);
                None
            }
        }
    }

    fn complete(&mut self, now_ms: u64) {
        self.state = MtuSearch::Complete;
        self.lost_in_row = 0;
        self.last_large_ack_ms = now_ms;
        self.raise_at_ms = now_ms.saturating_add(MTU_RAISE_INTERVAL_MS);
    }

    /// The probe left as packet `seq`; it is lost when not acknowledged by `deadline_ms`.
    pub(crate) fn probe_sent(&mut self, request: ProbeRequest, seq: u64, deadline_ms: u64) {
        self.counters.probes_sent += 1;
        self.outstanding = Some(Outstanding {
            seq,
            size: request.size,
            deadline_ms,
            confirm: request.confirm,
        });
    }

    /// The transport refused a probe of `request.size` bytes: sizes from it up are out of reach.
    pub(crate) fn probe_refused(&mut self, request: ProbeRequest, now_ms: u64) {
        self.ceiling = self.ceiling.min(request.size.saturating_sub(1));
        if request.confirm {
            self.black_hole(now_ms);
        } else {
            self.complete(now_ms);
        }
    }

    /// The outstanding probe's number, when its deadline passed (the caller declares it lost).
    pub(crate) fn expired(&self, now_ms: u64) -> Option<u64> {
        self.outstanding
            .filter(|o| now_ms >= o.deadline_ms)
            .map(|o| o.seq)
    }

    /// True when `seq` is the probe in flight.
    pub(crate) fn is_outstanding(&self, seq: u64) -> bool {
        self.outstanding.is_some_and(|o| o.seq == seq)
    }

    /// Packet `seq` (a probe) was acknowledged.
    pub(crate) fn probe_acked(&mut self, seq: u64, now_ms: u64) {
        let Some(probe) = self.outstanding.filter(|o| o.seq == seq) else {
            return;
        };
        self.outstanding = None;
        self.lost_in_row = 0;
        self.last_large_ack_ms = now_ms;
        self.loss_run = 0;
        if probe.confirm {
            self.doubt = false;
            self.next_probe_ms = now_ms.saturating_add(MTU_CONFIRM_MIN_GAP_MS);
            return;
        }
        self.plpmtu = self.plpmtu.max(probe.size);
        self.next_index += 1;
        self.next_probe_ms = now_ms;
        if self.next_index >= MTU_PROBE_SIZES.len() {
            self.complete(now_ms);
        }
    }

    /// The probe `seq` went unanswered and nothing sent after it was acknowledged either (an
    /// outage, not a verdict on its size): it leaves again at `retry_ms`.
    pub(crate) fn probe_inconclusive(&mut self, seq: u64, retry_ms: u64) {
        if self.outstanding.is_some_and(|o| o.seq == seq) {
            self.outstanding = None;
            self.next_probe_ms = retry_ms;
        }
    }

    /// Packet `seq` (a probe) was declared lost: a packet sent after it was acknowledged.
    pub(crate) fn probe_lost(&mut self, seq: u64, now_ms: u64) {
        let Some(probe) = self.outstanding.filter(|o| o.seq == seq) else {
            return;
        };
        self.outstanding = None;
        self.counters.probes_lost += 1;
        self.lost_in_row = self.lost_in_row.saturating_add(1);
        self.next_probe_ms = now_ms;
        if self.lost_in_row < MAX_MTU_PROBES {
            return;
        }
        if probe.confirm {
            self.black_hole(now_ms);
        } else {
            // The path does not carry this size: keep the confirmed one.
            self.complete(now_ms);
        }
    }

    /// A data packet of `size` bytes was acknowledged.
    pub(crate) fn packet_acked(&mut self, size: usize, now_ms: u64) {
        if size > self.base {
            self.last_large_ack_ms = now_ms;
            self.loss_run = 0;
        }
    }

    /// The number of the first large packet of the open loss run, if one is open.
    pub(crate) fn loss_run_first(&self) -> Option<u64> {
        (self.loss_run > 0).then_some(self.loss_run_first)
    }

    /// A data packet of `size` bytes (`seq`) was declared lost. `small_acked_between`: a packet
    /// at most the base size, sent after the run's first loss and before this one, was
    /// acknowledged (the path is up; during an outage every packet in between is lost too).
    /// Returns true when this was a black hole.
    pub(crate) fn packet_lost(
        &mut self,
        size: usize,
        seq: u64,
        small_acked_between: bool,
        now_ms: u64,
    ) -> bool {
        if !self.enabled() || size <= self.base {
            return false;
        }
        if self.loss_run == 0 {
            self.loss_run_alive = false;
            self.loss_run_first = seq;
        } else {
            self.loss_run_alive |= small_acked_between;
        }
        self.loss_run += 1;
        if self.plpmtu() > self.base && !self.doubt {
            // Data goes at the base size until a confirmation probe answers.
            self.doubt = true;
            // Confirmations count their own losses: earlier lost search probes do not.
            self.lost_in_row = 0;
            if self.outstanding.is_some_and(|o| !o.confirm) {
                // A search probe in flight says nothing about the confirmed size.
                self.outstanding = None;
            }
            self.next_probe_ms = self.next_probe_ms.min(now_ms);
        }
        if self.loss_run >= BLACK_HOLE_LOSS_BURST && self.loss_run_alive {
            self.black_hole(now_ms);
            return true;
        }
        false
    }

    fn black_hole(&mut self, now_ms: u64) {
        self.counters.black_holes += 1;
        self.plpmtu = self.base;
        self.state = MtuSearch::Error;
        self.outstanding = None;
        self.lost_in_row = 0;
        self.loss_run = 0;
        self.loss_run_alive = false;
        self.doubt = false;
        self.next_probe_ms = now_ms.saturating_add(MTU_BLACK_HOLE_RETRY_MS);
        tracing::info!(
            target: "regia_net",
            base = self.base,
            "path MTU black hole: back to the base packet size"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn searching() -> PathMtu {
        PathMtu::new(true, BASE_PLPMTU)
    }

    #[test]
    fn disabled_never_probes_and_keeps_the_base() {
        let mut p = PathMtu::new(false, BASE_PLPMTU);
        for t in [0, 1_000, 1_000_000] {
            assert_eq!(p.probe_due(t, true), None);
        }
        assert_eq!(p.plpmtu(), BASE_PLPMTU);
        assert!(!p.packet_lost(1452, 5, true, 0));
        assert_eq!(p.state().as_str(), "disabled");
    }

    #[test]
    fn no_probe_before_a_round_trip_sample() {
        let mut p = searching();
        assert_eq!(p.probe_due(0, false), None);
        assert_eq!(p.probe_due(0, true).map(|r| r.size), Some(1280));
    }

    #[test]
    fn acknowledged_probes_climb_and_three_losses_settle() {
        let mut p = searching();
        let r = p.probe_due(0, true).expect("probe");
        p.probe_sent(r, 1, 100);
        assert_eq!(p.probe_due(1, true), None, "one probe at a time");
        p.probe_acked(1, 50);
        assert_eq!(p.plpmtu(), 1280);
        for (i, seq) in (10..13).enumerate() {
            let r = p.probe_due(100 + i as u64 * 200, true).expect("probe");
            assert_eq!(r.size, 1400);
            p.probe_sent(r, seq, 0);
            assert_eq!(p.expired(1), Some(seq));
            p.probe_lost(seq, 100 + i as u64 * 200);
        }
        assert_eq!(p.state(), MtuSearch::Complete);
        assert_eq!(p.plpmtu(), 1280, "settles at the last confirmed size");
        assert_eq!(p.counters().probes_lost, 3);
        // 1,452 is never tried while the 1,400 search is complete and nothing is lost.
        assert_eq!(p.probe_due(2_000, true), None);
    }

    #[test]
    fn three_lost_confirmations_are_a_black_hole() {
        let mut p = searching();
        for (seq, size) in [(1, 1280), (2, 1400), (3, 1452)] {
            let r = p.probe_due(0, true).expect("probe");
            assert_eq!(r.size, size);
            p.probe_sent(r, seq, 100);
            p.probe_acked(seq, 0);
        }
        assert_eq!((p.state(), p.plpmtu()), (MtuSearch::Complete, 1452));
        for seq in 10..13 {
            let r = p.probe_due(MTU_CONFIRM_INTERVAL_MS + seq, true).expect("confirm");
            assert!(r.confirm);
            assert_eq!(r.size, 1452);
            p.probe_sent(r, seq, 0);
            p.probe_lost(seq, MTU_CONFIRM_INTERVAL_MS + seq);
        }
        assert_eq!((p.state(), p.plpmtu()), (MtuSearch::Error, BASE_PLPMTU));
        assert_eq!(p.counters().black_holes, 1);
    }

    #[test]
    fn a_large_loss_burst_on_a_live_path_is_a_black_hole_and_an_outage_is_not() {
        let mut p = searching();
        let r = p.probe_due(0, true).expect("probe");
        p.probe_sent(r, 1, 100);
        p.probe_acked(1, 0);
        // An outage: everything lost, nothing smaller acknowledged in between. The size is in
        // doubt (data at the base) but not dropped.
        for seq in 10..20 {
            assert!(!p.packet_lost(1280, seq, false, 0));
        }
        assert!(p.in_doubt());
        assert_eq!((p.plpmtu(), p.packet_size()), (1280, BASE_PLPMTU));
        // The confirmation goes unanswered with nothing later acknowledged: inconclusive.
        let r = p.probe_due(1, true).expect("confirm");
        assert!(r.confirm);
        p.probe_sent(r, 20, 100);
        p.probe_inconclusive(20, 200);
        assert_eq!(p.counters().probes_lost, 0);
        let r = p.probe_due(200, true).expect("again");
        p.probe_sent(r, 21, 300);
        p.probe_acked(21, 250);
        assert!(!p.in_doubt());
        assert_eq!(p.packet_size(), 1280);
        // Large packets lost while small ones get through.
        assert!(!p.packet_lost(1280, 30, false, 0));
        p.packet_acked(1200, 0);
        assert!(!p.packet_lost(1280, 32, true, 0));
        assert!(p.packet_lost(1280, 34, true, 0));
        assert_eq!(p.plpmtu(), BASE_PLPMTU);
    }

    /// Lost search probes do not count toward the confirmations a doubt sends: two lost 1,400
    /// probes, then a large loss, then one lost confirmation is not a black hole.
    #[test]
    fn search_losses_do_not_count_against_confirmations() {
        let mut p = searching();
        let r = p.probe_due(0, true).expect("probe");
        p.probe_sent(r, 1, 100);
        p.probe_acked(1, 0);
        for seq in [2, 3] {
            let r = p.probe_due(10, true).expect("1,400");
            assert_eq!(r.size, 1400);
            p.probe_sent(r, seq, 100);
            p.probe_lost(seq, 10);
        }
        assert!(!p.packet_lost(1280, 4, false, 20));
        let r = p.probe_due(20, true).expect("confirm");
        assert!(r.confirm);
        p.probe_sent(r, 5, 200);
        p.probe_lost(5, 30);
        assert_eq!(p.counters().black_holes, 0);
        assert_eq!(p.plpmtu(), 1280);
    }

    #[test]
    fn a_refused_probe_caps_the_search() {
        let mut p = searching();
        p.set_ceiling(1300);
        let r = p.probe_due(0, true).expect("probe");
        p.probe_sent(r, 1, 100);
        p.probe_acked(1, 0);
        assert_eq!(p.probe_due(1, true), None, "1,400 is above the ceiling");
        assert_eq!((p.state(), p.plpmtu()), (MtuSearch::Complete, 1280));
    }
}
