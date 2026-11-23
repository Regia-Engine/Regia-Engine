// SPDX-License-Identifier: MPL-2.0
//! The host check before hosting (spec 120 § 10.7, the DO27 hosting flow, plan N229, ADR-8370):
//! did the port mapping work, can a stranger reach the mapped port, and how many clients does the
//! uplink carry.
//!
//! - [`PortForwardCheck`]: asks the relay (protocol 3) to send a probe to the host's mapped
//!   address from a relay port the host never sent to, and waits for it on the game socket.
//!   Reachable when it arrives, unreachable when the relay sent it and nothing came.
//! - [`UplinkProbe`]: measures the host's uplink toward the relay with a bounded startup in the
//!   shape of BBR's (Cardwell et al., "BBR: Congestion-Based Congestion Control", ACM Queue 2016):
//!   the pacing rate starts at 256 KiB/s (spec 120 § 4.2 item 6) and grows by 2/ln 2 of the
//!   delivery rate each round trip until the delivery rate stops growing by 25 % for three rounds.
//!   Delivery-rate samples follow draft-cheng-iccrg-delivery-rate-estimation (the larger of the
//!   send and ack intervals; samples shorter than the minimum RTT are discarded).
//! - [`estimate_clients`]: the measured uplink divided by the per-client budget.
//!
//! Spec 120 § 10.7 names "the rate controller's startup" as the measurement; that controller
//! (`cc.rs`, § 4.2) is not on this line yet, so the startup here is its own and bounded by
//! [`UPLINK_PROBE_MAX_MS`] and [`UPLINK_PROBE_MAX_BYTES`]. Sans-IO and sans-clock: the caller passes
//! milliseconds and moves the datagrams (`server_host::RelayRegistrar` does it on the game socket).

use std::collections::VecDeque;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::port_map::PortMapping;
use crate::relay::{
    RELAY_MEASURE_BYTES, RELAY_MEASURE_BYTES_PER_WINDOW, RELAY_PROBE_NONCE_BYTES, RelayReply, encode_measure,
    encode_probe_request,
};

/// How often an unacknowledged probe request is sent again, milliseconds.
pub const HOST_CHECK_PROBE_RETRY_MS: u64 = 250;
/// Probe requests a check sends at most.
pub const HOST_CHECK_PROBE_ATTEMPTS: u32 = 4;
/// How long a check waits for the probe after the relay said it sent one, milliseconds.
pub const HOST_CHECK_PROBE_WAIT_MS: u64 = 1_000;
/// Initial pacing rate of the uplink startup, bytes per second (spec 120 § 4.2 item 6).
pub const UPLINK_PROBE_START_BYTES_PER_SECOND: f64 = 256.0 * 1024.0;
/// Pacing gain of the startup, `2 / ln 2` (BBR's startup gain).
pub const UPLINK_PROBE_STARTUP_GAIN: f64 = 2.885;
/// Growth that counts as "the delivery rate still grows" per round (BBR: 25 %).
pub const UPLINK_PROBE_FULL_GROWTH: f64 = 1.25;
/// Rounds without that growth that end the startup (BBR: three).
pub const UPLINK_PROBE_FULL_ROUNDS: u32 = 3;
/// Longest measurement, milliseconds.
pub const UPLINK_PROBE_MAX_MS: u64 = 3_000;
/// Most bytes one measurement sends (the relay's budget per host and window).
pub const UPLINK_PROBE_MAX_BYTES: u64 = RELAY_MEASURE_BYTES_PER_WINDOW;
/// A measurement with no acknowledgement for this long ends, milliseconds.
pub const UPLINK_PROBE_SILENCE_MS: u64 = 1_000;
/// Bytes the pacer may send in one burst: what the pacing rate earns in this many milliseconds
/// (at least two packets), so a host that polls once a tick still reaches its rate.
pub const UPLINK_PROBE_BURST_MS: u64 = 20;
/// Packets the probe remembers at most while waiting for their acknowledgement.
const UPLINK_PROBE_MAX_INFLIGHT: usize = 16_384;

/// Whether the host's mapped port takes datagrams from a stranger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reachability {
    /// No mapping to check (the race failed or did not run).
    NoMapping,
    /// The check is running.
    Checking,
    /// The probe arrived through the mapping.
    Reachable,
    /// The relay sent the probe and it never arrived.
    Unreachable,
    /// The relay has no probe port, so it cannot check.
    RelayCannotProbe,
    /// The relay never acknowledged a probe request.
    RelayNoAnswer,
    /// The relay speaks a protocol older than 3.
    RelayTooOld,
}

impl Reachability {
    /// Stable lowercase name (agent protocol, logs).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoMapping => "no_mapping",
            Self::Checking => "checking",
            Self::Reachable => "reachable",
            Self::Unreachable => "unreachable",
            Self::RelayCannotProbe => "relay_cannot_probe",
            Self::RelayNoAnswer => "relay_no_answer",
            Self::RelayTooOld => "relay_too_old",
        }
    }

    /// The check has its answer.
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Checking)
    }
}

/// The port-forward self-check: the relay probes `target` and the game socket listens.
#[derive(Debug, Clone)]
pub struct PortForwardCheck {
    target: SocketAddr,
    nonce: [u8; RELAY_PROBE_NONCE_BYTES],
    sent: u32,
    next_send_ms: u64,
    last_ack_ms: Option<u64>,
    state: Reachability,
}

impl PortForwardCheck {
    /// A check of `target` (the mapped address) with a fresh nonce, starting at `now_ms`.
    pub fn new(target: SocketAddr, now_ms: u64) -> Self {
        let secret = crate::handshake::fresh_secret();
        let mut nonce = [0u8; RELAY_PROBE_NONCE_BYTES];
        nonce.copy_from_slice(&secret[..RELAY_PROBE_NONCE_BYTES]);
        Self {
            target,
            nonce,
            sent: 0,
            next_send_ms: now_ms,
            last_ack_ms: None,
            state: Reachability::Checking,
        }
    }

    /// The address being checked.
    pub fn target(&self) -> SocketAddr {
        self.target
    }

    /// Where the check stands.
    pub fn state(&self) -> Reachability {
        self.state
    }

    /// The next probe request to send to the relay, when one is due; also settles the answer
    /// once its wait has passed.
    pub fn poll(&mut self, now_ms: u64) -> Option<Vec<u8>> {
        if self.state != Reachability::Checking {
            return None;
        }
        if let Some(acked) = self.last_ack_ms
            && self.sent >= HOST_CHECK_PROBE_ATTEMPTS
            && now_ms >= acked.saturating_add(HOST_CHECK_PROBE_WAIT_MS)
        {
            self.state = Reachability::Unreachable;
            return None;
        }
        if now_ms < self.next_send_ms {
            return None;
        }
        if self.sent >= HOST_CHECK_PROBE_ATTEMPTS {
            if self.last_ack_ms.is_none() {
                self.state = Reachability::RelayNoAnswer;
            }
            return None;
        }
        self.sent += 1;
        self.next_send_ms = now_ms.saturating_add(if self.sent >= HOST_CHECK_PROBE_ATTEMPTS {
            HOST_CHECK_PROBE_WAIT_MS
        } else {
            HOST_CHECK_PROBE_RETRY_MS
        });
        Some(encode_probe_request(&self.nonce, self.target))
    }

    /// A reply from the relay's control port: the acknowledgement, or a protocol refusal.
    pub fn on_reply(&mut self, reply: &RelayReply, now_ms: u64) {
        if self.state != Reachability::Checking {
            return;
        }
        match reply {
            RelayReply::ProbeSent { nonce, sent } if *nonce == self.nonce => {
                if *sent {
                    self.last_ack_ms = Some(now_ms);
                } else {
                    self.state = Reachability::RelayCannotProbe;
                }
            }
            RelayReply::VersionTooNew { .. } | RelayReply::VersionTooOld { .. } => {
                self.state = Reachability::RelayTooOld;
            }
            _ => {}
        }
    }

    /// A probe that arrived on the game socket from a relay port other than its control port.
    pub fn on_probe(&mut self, nonce: &[u8; RELAY_PROBE_NONCE_BYTES]) {
        if self.state == Reachability::Checking && crate::wire::constant_time_eq(nonce, &self.nonce) {
            self.state = Reachability::Reachable;
        }
    }
}

/// Why an uplink measurement ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UplinkEnd {
    /// Still running.
    Running,
    /// The delivery rate stopped growing (the pipe is full).
    FullPipe,
    /// The time or byte cap came first: the uplink is at least the estimate.
    Capped,
    /// No acknowledgement came: the relay does not measure (or nothing gets through).
    NoAnswer,
    /// The relay speaks a protocol older than 3.
    RelayTooOld,
}

impl UplinkEnd {
    /// Stable lowercase name (agent protocol, logs).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::FullPipe => "full_pipe",
            Self::Capped => "capped",
            Self::NoAnswer => "relay_no_answer",
            Self::RelayTooOld => "relay_too_old",
        }
    }
}

/// One measurement packet in flight.
#[derive(Debug, Clone, Copy)]
struct Sent {
    seq: u32,
    send_ms: u64,
    /// Bytes delivered when it left.
    delivered: u64,
    /// When `delivered` last grew, as it left.
    delivered_ms: u64,
    /// The send time of the packet most recently acknowledged, as it left.
    first_sent_ms: u64,
}

/// The uplink measurement: a bounded startup against the relay's measurement echo.
#[derive(Debug, Clone)]
pub struct UplinkProbe {
    start_ms: u64,
    pacing: f64,
    credit: f64,
    credit_ms: u64,
    next_seq: u32,
    inflight: VecDeque<Sent>,
    delivered: u64,
    delivered_ms: u64,
    first_sent_ms: u64,
    next_round_delivered: u64,
    rounds: u32,
    full_bw: f64,
    full_bw_rounds: u32,
    max_bw: f64,
    min_rtt_ms: Option<u64>,
    last_ack_ms: u64,
    sent_bytes: u64,
    acked: u64,
    end: UplinkEnd,
}

impl UplinkProbe {
    /// A measurement starting at `now_ms`.
    pub fn new(now_ms: u64) -> Self {
        Self {
            start_ms: now_ms,
            pacing: UPLINK_PROBE_START_BYTES_PER_SECOND,
            credit: RELAY_MEASURE_BYTES as f64,
            credit_ms: now_ms,
            next_seq: 0,
            inflight: VecDeque::new(),
            delivered: 0,
            delivered_ms: now_ms,
            first_sent_ms: now_ms,
            next_round_delivered: 0,
            rounds: 0,
            full_bw: 0.0,
            full_bw_rounds: 0,
            max_bw: 0.0,
            min_rtt_ms: None,
            last_ack_ms: now_ms,
            sent_bytes: 0,
            acked: 0,
            end: UplinkEnd::Running,
        }
    }

    /// Why it ended, or `Running`.
    pub fn end(&self) -> UplinkEnd {
        self.end
    }

    /// The measured uplink, bytes per second: the largest delivery-rate sample (BBR's bottleneck
    /// bandwidth filter); `None` before the first sample.
    pub fn estimate_bytes_per_second(&self) -> Option<u64> {
        (self.max_bw > 0.0).then_some(self.max_bw as u64)
    }

    /// Bytes sent so far.
    pub fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    /// Packets acknowledged so far.
    pub fn acked(&self) -> u64 {
        self.acked
    }

    /// Round trips the startup ran.
    pub fn rounds(&self) -> u32 {
        self.rounds
    }

    /// The smallest round-trip time seen, milliseconds.
    pub fn min_rtt_ms(&self) -> Option<u64> {
        self.min_rtt_ms
    }

    fn finish(&mut self, end: UplinkEnd) {
        if self.end == UplinkEnd::Running {
            self.end = end;
            self.inflight.clear();
        }
    }

    /// The next measurement packet the pacer allows now, if any; call until `None` each update.
    pub fn poll(&mut self, now_ms: u64) -> Option<Vec<u8>> {
        if self.end != UplinkEnd::Running {
            return None;
        }
        if now_ms.saturating_sub(self.last_ack_ms) >= UPLINK_PROBE_SILENCE_MS {
            self.finish(if self.acked == 0 {
                UplinkEnd::NoAnswer
            } else {
                UplinkEnd::Capped
            });
            return None;
        }
        if now_ms.saturating_sub(self.start_ms) >= UPLINK_PROBE_MAX_MS
            || self.sent_bytes + RELAY_MEASURE_BYTES as u64 > UPLINK_PROBE_MAX_BYTES
        {
            self.finish(UplinkEnd::Capped);
            return None;
        }
        let size = RELAY_MEASURE_BYTES as f64;
        if now_ms > self.credit_ms {
            let earned = self.pacing * (now_ms - self.credit_ms) as f64 / 1000.0;
            let burst = (self.pacing * UPLINK_PROBE_BURST_MS as f64 / 1000.0).max(2.0 * size);
            self.credit = (self.credit + earned).min(burst);
            self.credit_ms = now_ms;
        }
        if self.credit < size {
            return None;
        }
        self.credit -= size;
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.inflight.is_empty() {
            // Nothing in flight: the send interval of the next sample starts now.
            self.first_sent_ms = now_ms;
            self.delivered_ms = now_ms;
        }
        if self.inflight.len() >= UPLINK_PROBE_MAX_INFLIGHT {
            self.inflight.pop_front();
        }
        self.inflight.push_back(Sent {
            seq,
            send_ms: now_ms,
            delivered: self.delivered,
            delivered_ms: self.delivered_ms,
            first_sent_ms: self.first_sent_ms,
        });
        self.sent_bytes += RELAY_MEASURE_BYTES as u64;
        Some(encode_measure(seq))
    }

    /// A reply from the relay's control port: an acknowledgement or a protocol refusal.
    pub fn on_reply(&mut self, reply: &RelayReply, now_ms: u64) {
        match reply {
            RelayReply::MeasureAck { seq, capped } => {
                self.on_ack(*seq, now_ms);
                if *capped {
                    self.finish(UplinkEnd::Capped);
                }
            }
            RelayReply::VersionTooNew { .. } | RelayReply::VersionTooOld { .. } => {
                self.finish(UplinkEnd::RelayTooOld);
            }
            _ => {}
        }
    }

    /// Packet `seq` was acknowledged at `now_ms`: one delivery-rate sample, and the round count.
    pub fn on_ack(&mut self, seq: u32, now_ms: u64) {
        if self.end != UplinkEnd::Running {
            return;
        }
        let Ok(i) = self.inflight.binary_search_by_key(&seq, |p| p.seq) else {
            return;
        };
        let Some(p) = self.inflight.remove(i) else {
            return;
        };
        // Older packets still unacknowledged are lost (the relay answers in order of arrival).
        self.inflight.drain(..i);
        self.acked += 1;
        self.last_ack_ms = now_ms;
        self.delivered += RELAY_MEASURE_BYTES as u64;
        self.delivered_ms = now_ms;
        let rtt = now_ms.saturating_sub(p.send_ms);
        self.min_rtt_ms = Some(self.min_rtt_ms.map_or(rtt, |m| m.min(rtt)));
        let send_elapsed = p.send_ms.saturating_sub(p.first_sent_ms);
        let ack_elapsed = now_ms.saturating_sub(p.delivered_ms);
        self.first_sent_ms = p.send_ms;
        let interval = send_elapsed.max(ack_elapsed);
        if interval > 0 && interval >= self.min_rtt_ms.unwrap_or(0) {
            let rate = (self.delivered - p.delivered) as f64 * 1000.0 / interval as f64;
            self.max_bw = self.max_bw.max(rate);
        }
        if p.delivered >= self.next_round_delivered {
            self.next_round_delivered = self.delivered;
            self.rounds += 1;
            self.end_of_round();
        }
    }

    /// BBR's full-pipe check and the startup's pacing rate.
    fn end_of_round(&mut self) {
        if self.max_bw >= self.full_bw * UPLINK_PROBE_FULL_GROWTH {
            self.full_bw = self.max_bw;
            self.full_bw_rounds = 0;
        } else {
            self.full_bw_rounds += 1;
            if self.full_bw_rounds >= UPLINK_PROBE_FULL_ROUNDS {
                self.finish(UplinkEnd::FullPipe);
                return;
            }
        }
        self.pacing = (UPLINK_PROBE_STARTUP_GAIN * self.max_bw).max(UPLINK_PROBE_START_BYTES_PER_SECOND);
    }
}

/// Clients an uplink of `uplink_bytes_per_second` carries at `per_client_bytes_per_second` each.
pub fn estimate_clients(uplink_bytes_per_second: u64, per_client_bytes_per_second: u32) -> u32 {
    if per_client_bytes_per_second == 0 {
        return 0;
    }
    (uplink_bytes_per_second / u64::from(per_client_bytes_per_second)).min(u64::from(u32::MAX)) as u32
}

/// The granted mapping as an agent sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingView {
    /// The mapper that won the race: `pcp`, `nat_pmp` or `upnp`.
    pub mapper: String,
    /// The public address clients use.
    pub external: String,
    /// The game socket's port it forwards to.
    pub internal_port: u16,
    /// Seconds the gateway granted.
    pub lifetime_s: u32,
    /// Renewals so far.
    pub renewals: u64,
}

impl MappingView {
    /// The view of `mapping` granted by `mapper`.
    pub fn new(mapper: &str, mapping: &PortMapping, renewals: u64) -> Self {
        Self {
            mapper: mapper.to_owned(),
            external: mapping.external.to_string(),
            internal_port: mapping.internal_port,
            lifetime_s: mapping.lifetime_secs,
            renewals,
        }
    }
}

/// The host check as `net.host_check` reports it (spec 120 § 10.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostCheckReport {
    /// The port mapping, once one was granted.
    pub mapping: Option<MappingView>,
    /// Why there is no mapping: `port_map_refused`, `port_map_unavailable`, `port_map_off`, or
    /// `None` while the race runs or once a mapping exists.
    pub mapping_error: Option<String>,
    /// The port-forward self-check: `no_mapping`, `checking`, `reachable`, `unreachable`,
    /// `relay_cannot_probe`, `relay_no_answer` or `relay_too_old`.
    pub reachable: String,
    /// The measured uplink toward the relay, bytes per second.
    pub uplink_bytes_per_second: Option<u64>,
    /// How the measurement ended: `running`, `full_pipe`, `capped` (the uplink is at least the
    /// estimate), `relay_no_answer` or `relay_too_old`; `None` when it did not run.
    pub uplink_end: Option<String>,
    /// The per-client budget the estimate divides by, bytes per second.
    pub per_client_bytes_per_second: u32,
    /// Clients the uplink carries at that budget.
    pub estimated_clients: Option<u32>,
    /// Every part has its answer.
    pub done: bool,
}

impl Default for HostCheckReport {
    fn default() -> Self {
        Self {
            mapping: None,
            mapping_error: None,
            reachable: Reachability::NoMapping.as_str().to_owned(),
            uplink_bytes_per_second: None,
            uplink_end: None,
            per_client_bytes_per_second: 0,
            estimated_clients: None,
            done: false,
        }
    }
}

/// What the host check runs (`RelayRegistrar::start_host_check`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCheckConfig {
    /// The per-client budget the capacity estimate divides by, bytes per second (the session's
    /// `peer_bytes_per_second`, 256 KiB/s by default).
    pub per_client_bytes_per_second: u32,
    /// Measure the uplink (sends up to [`UPLINK_PROBE_MAX_BYTES`] to the relay).
    pub measure_uplink: bool,
}

impl Default for HostCheckConfig {
    fn default() -> Self {
        Self {
            per_client_bytes_per_second: 262_144,
            measure_uplink: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(nonce: [u8; RELAY_PROBE_NONCE_BYTES], sent: bool) -> RelayReply {
        RelayReply::ProbeSent { nonce, sent }
    }

    #[test]
    fn a_probe_that_arrives_is_reachable_and_one_that_never_does_is_not() {
        let target = SocketAddr::from(([203, 0, 113, 5], 7777));
        let mut check = PortForwardCheck::new(target, 0);
        assert!(check.poll(0).is_some());
        let nonce = check.nonce;
        check.on_reply(&ack(nonce, true), 20);
        check.on_probe(&[0; RELAY_PROBE_NONCE_BYTES]);
        assert_eq!(check.state(), Reachability::Checking, "a wrong nonce is ignored");
        check.on_probe(&nonce);
        assert_eq!(check.state(), Reachability::Reachable);

        let mut check = PortForwardCheck::new(target, 0);
        let mut sends = 0;
        let mut now = 0;
        while !check.state().is_final() && now < 10_000 {
            if check.poll(now).is_some() {
                sends += 1;
                check.on_reply(&ack(check.nonce, true), now + 20);
            }
            now += 10;
        }
        assert_eq!(check.state(), Reachability::Unreachable);
        assert_eq!(sends, HOST_CHECK_PROBE_ATTEMPTS);
        // The last acknowledgement plus the wait.
        assert!(now <= 750 + 20 + HOST_CHECK_PROBE_WAIT_MS + 20, "{now}");
    }

    #[test]
    fn a_relay_that_cannot_probe_or_never_answers_says_so() {
        let target = SocketAddr::from(([203, 0, 113, 5], 7777));
        let mut check = PortForwardCheck::new(target, 0);
        assert!(check.poll(0).is_some());
        check.on_reply(&ack(check.nonce, false), 5);
        assert_eq!(check.state(), Reachability::RelayCannotProbe);

        let mut check = PortForwardCheck::new(target, 0);
        let mut now = 0;
        while !check.state().is_final() && now < 10_000 {
            let _ = check.poll(now);
            now += 10;
        }
        assert_eq!(check.state(), Reachability::RelayNoAnswer);

        let mut check = PortForwardCheck::new(target, 0);
        let _ = check.poll(0);
        check.on_reply(&RelayReply::VersionTooNew { max: 2 }, 5);
        assert_eq!(check.state(), Reachability::RelayTooOld);
    }

    #[test]
    fn capacity_is_the_uplink_over_the_budget() {
        assert_eq!(estimate_clients(2_500_000, 262_144), 9);
        assert_eq!(estimate_clients(2_500_000, 65_536), 38);
        assert_eq!(estimate_clients(100, 0), 0);
    }

    /// With nobody answering, the measurement ends after the silence and has no estimate.
    #[test]
    fn a_silent_relay_ends_the_measurement_without_an_estimate() {
        let mut probe = UplinkProbe::new(0);
        let mut sent = 0;
        for now in 0..5_000 {
            while probe.poll(now).is_some() {
                sent += 1;
            }
        }
        assert_eq!(probe.end(), UplinkEnd::NoAnswer);
        assert_eq!(probe.estimate_bytes_per_second(), None);
        // About one second at the 256 KiB/s start rate.
        assert!(sent <= 260 * 1024 / RELAY_MEASURE_BYTES + 2, "{sent}");
    }
}
