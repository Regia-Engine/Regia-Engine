// SPDX-License-Identifier: MPL-2.0
//! The send-rate controller, the pacer and the shared uplink (spec 120 § 4.2, ADR-8301).
//!
//! [`RateController`] is a game rate controller in the GCC family (draft-ietf-rmcat-gcc): it is
//! fed one [`RoundSample`] per feedback round (at most one decision per `max(srtt, 50 ms)`) and
//! answers with a target rate. Its signals are a BBR-style delivery rate (bytes newly
//! acknowledged over the round, Cardwell et al. 2016), the queueing delay over the minimum round
//! trip, the one-way delay of its own packets from the arrival stamps of wire 6 ([`OneWayDelay`]:
//! a sender that uses little of its target is cut for a queue only when its own packets met one
//! on the way out, ADR-10050), the one-way delay gradient of a [`TrendlineFilter`] when receive
//! timestamps are negotiated, and the round's loss. Loss cuts the rate only with evidence of congestion:
//! random wireless loss on an underused path is answered by redundancy, not by a cut (F7).
//!
//! [`Pacer`] spreads datagrams at 1.25x the target (RFC 9002 § 7.7): the first burst is the
//! initial window, later bursts are at most one tick of the pacing rate (never under
//! [`PACING_BURST_PACKETS`] packets). [`SharedUplink`] shares one host uplink between the peers
//! of a listen server with weighted max-min fairness (F5). Everything here is pure: time is passed in, nothing reads a
//! clock, so every decision replays from its inputs.

use serde::{Deserialize, Serialize};

/// Queueing delay over the minimum round trip that counts as a standing queue, milliseconds
/// (spec 120 § 4.2 item 2).
pub const QUEUE_DELAY_THRESHOLD_MS: f64 = 25.0;
/// Shortest feedback round, milliseconds (item 1: one decision per `max(srtt, 50 ms)`).
pub const MIN_ROUND_MS: u64 = 50;
/// The decrease factor applied to the delivery rate on overuse (item 2).
pub const DECREASE_FACTOR: f64 = 0.85;
/// Multiplicative increase far from the last cut, per second (item 2).
pub const INCREASE_PER_SECOND: f64 = 0.08;
/// Pacing gain over the target (item 7).
pub const PACING_GAIN: f64 = 1.25;
/// Packets a later burst may carry at least (item 7): four, so a 4 KiB reliable message an idle
/// sender queues leaves in one poll instead of trickling out at an app-limited target (ADR-8692;
/// RFC 9002 § 7.7 allows bursts up to the initial window). It was two. Also the packets a
/// foreground reliable message may take in the poll that starts it, and the depth of the excess
/// allowance that burst draws on when the bucket is spent (ADR-8859).
pub const PACING_BURST_PACKETS: usize = 4;
/// Bytes every datagram costs on the wire in front of the endpoint's own header, as
/// [`WireShare`] counts them (ADR-10097): an IPv4 header without options and a UDP header
/// (RFC 791, RFC 768). A path over IPv6 costs 20 bytes more per datagram, which is under-counted.
pub const DATAGRAM_OVERHEAD_BYTES: usize = 28;
/// The memory of a [`WireShare`], milliseconds: a packet sent this long ago weighs half.
pub const WIRE_SHARE_MEMORY_MS: f64 = 1_000.0;
/// Loss below this fraction has no effect (item 3).
pub const LOSS_IGNORE: f64 = 0.02;
/// Loss at or above this fraction cuts the rate when the path also shows congestion (item 3).
pub const LOSS_CUT: f64 = 0.10;
/// Loss over this fraction, while even the floor does not get through, trips the circuit breaker
/// (item 4).
pub const BREAKER_LOSS: f64 = 0.5;
/// After a loss-driven cut the rate climbs back to its pre-cut value within this long once the loss
/// clears (item 5, DO27 rule A4: never slower than the cut).
pub const LOSS_RECOVERY_MS: u64 = 2_000;
/// The smallest target the controller ever sets, bytes per second (two 1200-byte packets per
/// 600 ms): the essential class keeps its own floor on top.
pub const MIN_TARGET_BYTES_PER_SECOND: u32 = 4_096;
/// What the target keeps above the essential class's own rate, bytes per second (ADR-8534): the
/// share of the rest (the reliable stream's repairs and catch-ups first, spec 120 § 4.2 item 9)
/// when a cut would take the target under what the essential class sends anyway.
pub const ESSENTIAL_RESERVE_BYTES_PER_SECOND: u32 = MIN_TARGET_BYTES_PER_SECOND;
/// Weight of the latest round in the smoothed essential rate (about four rounds of memory).
const ESSENTIAL_RATE_GAIN: f64 = 0.25;
/// Rounds whose smallest round-trip sample the queueing delay is read from.
pub const QUEUE_FLOOR_ROUNDS: usize = 2;
/// Acknowledged packets the queue evidence of a loss cut spans at least (ADR-8490): the smallest
/// round trip of a few packets sits over the path's minimum by the jitter of both directions, so
/// the reading reaches back over the latest rounds (at least [`QUEUE_FLOOR_ROUNDS`], at most
/// `QUEUE_EVIDENCE_MAX_ROUNDS`) until they hold this many.
const QUEUE_EVIDENCE_PACKETS: u32 = 32;
/// Rounds the queue readings reach back over at most.
const QUEUE_EVIDENCE_MAX_ROUNDS: usize = 8;
/// The window a one-way delay's minimum is kept over, milliseconds: the round trip minimum's
/// (`connection::MIN_RTT_WINDOW_MS`).
pub const ONE_WAY_WINDOW_MS: u64 = 10_000;
/// A gap between arrival stamps this long restarts the one-way delay measurement, milliseconds:
/// a 16-bit millisecond stamp unwraps only within half its span (32.768 s), and the estimate of
/// the peer's clock runs on this side's.
pub const ONE_WAY_RESTART_MS: u64 = 20_000;
/// Where [`OneWayDelay`] places its first sample, milliseconds: its scale has no meaningful
/// origin (the two clocks have none in common), and this keeps every later sample positive
/// whatever the clocks do.
const ONE_WAY_ORIGIN_MS: f64 = 1_048_576.0;
/// The one-way queueing delay that confirms an app-limited sender's overuse verdict, milliseconds
/// (ADR-10050): half of [`QUEUE_DELAY_THRESHOLD_MS`], rounded down. A sender that uses less than
/// half its target is cut for a queue only when the smallest one-way delay of its own latest
/// packets (over at least [`ONE_WAY_CONFIRM_SAMPLES`] samples) sits this far over the minimum.
pub const ONE_WAY_CONFIRM_MS: f64 = 12.0;
/// Delay samples the confirming one-way reading spans at least (ADR-10050). A sample is one
/// acknowledgment frame's newest packet and carries the phase of the receiver's socket reads: up
/// to one send interval of the sender plus the path's jitter (27 ms for a 60 Hz sender on a path
/// with 10 ms of jitter). A real queue delays every sample; the smallest of six phase-delayed
/// ones is over [`ONE_WAY_CONFIRM_MS`] in about 3 % of readings.
pub const ONE_WAY_CONFIRM_SAMPLES: u32 = 6;
/// Acknowledged packets the threshold detector's queue reading spans at least while the path
/// loses packets (smoothed loss at or over [`LOSS_IGNORE`], ADR-8490): loss thins the round-trip
/// samples (Karn's rule) and a lossy, jittery path read its jitter as a growing queue. A lossless
/// path keeps the round's own reading, which reacts within the round.
const QUEUE_DETECTOR_LOSSY_PACKETS: u32 = 64;
/// Growth stops at this fraction of the bottleneck estimate (ADR-8490): the delivery rate of the
/// latest [`DELIVERY_WINDOW_MS`] when a cut found a queue standing at the path's bottleneck. A
/// sender that grew to the full rate of a drop-tail buffer offered more than the link carries in
/// any second its growth crossed it; at 97 % the congested soak row kept every whole second under
/// the link's rate on 193 of 200 seeds (142 failed without it).
pub const BOTTLENECK_HEADROOM: f64 = 0.97;
/// The bottleneck estimate holds this long after the cut that measured it, milliseconds; then
/// growth tests the path again (a link that got faster is found, and a new queue re-measures it).
pub const BOTTLENECK_HOLD_MS: u64 = 15_000;
/// Rounds a congestion cut's recovery sends at the headroom under the path's measured rate with no
/// queue before it owes the rest of its pre-cut rate: the loss was transient (ADR-8534).
pub const RECOVERY_CLEAN_ROUNDS: u32 = 4;
/// A congestion cut that finds at least this much queue standing at the bottleneck drains it
/// (ADR-10145), milliseconds: four times [`QUEUE_DELAY_THRESHOLD_MS`]. Under it the cut's own
/// [`DECREASE_FACTOR`] empties the queue in about half a second (item 2 stands as it is).
pub const DRAIN_QUEUE_MS: f64 = 100.0;
/// The queue depth a drain's rate is scaled over, milliseconds (ADR-10145): the drain targets
/// `1 - queue / DRAIN_HORIZON_MS` of the bottleneck's delivery rate, between
/// [`DRAIN_GAIN_MIN`] and [`DECREASE_FACTOR`], so the deeper the queue the less is sent into it.
pub const DRAIN_HORIZON_MS: f64 = 400.0;
/// The smallest fraction of the bottleneck's delivery rate a drain targets (ADR-10145). BBR
/// drains its startup's queue at 0.35 of the bottleneck rate (Cardwell et al. 2016, the inverse
/// of its startup gain); the pacer here spreads at [`PACING_GAIN`] over the target, so 0.25
/// offers the path at most 0.31 of its rate and the queue empties at 0.69 or more.
pub const DRAIN_GAIN_MIN: f64 = 0.25;
/// A drain is over once a round's smallest queue reading is under this, milliseconds
/// (ADR-10145): [`ONE_WAY_CONFIRM_MS`], half the standing-queue threshold.
pub const DRAIN_DONE_MS: f64 = ONE_WAY_CONFIRM_MS;
/// One drain lasts at most this long, milliseconds (ADR-10145); a queue that is still falling
/// then is drained again from its new depth.
pub const DRAIN_MAX_MS: u64 = 1_500;
/// A drain that left the queue at this fraction of its depth or more did not move it: the queue
/// is not this sender's (cross traffic), and no drain starts for [`DRAIN_RETRY_MS`] (ADR-10145).
pub const DRAIN_UNMOVED: f64 = 0.8;
/// How long after a drain that did not move the queue no other one starts, milliseconds.
pub const DRAIN_RETRY_MS: u64 = BOTTLENECK_HOLD_MS;
/// The first burst of a connection carries at most this many milliseconds of a lowered start
/// rate (ADR-10145), or of its first flight's rate when `RateConfig::first_flight` is on
/// (ADR-10156: 300 ms of the 32 KiB/s floor is eight 1,200-byte packets, under RFC 6928's initial
/// window of ten), and never fewer than [`PACING_BURST_PACKETS`] packets
/// ([`RateConfig::start_burst_bytes`]). The default start rate (256 KiB/s, the most there is)
/// sets no bound: the first burst is the initial window the link is configured with, as before.
/// A project that lowers `start_bytes_per_second` for thin links lowers the first burst with it
/// (32 KiB/s: eight packets), where the start rate alone left the whole initial window in front
/// of it.
pub const START_BURST_MS: u64 = 300;
/// A path fell behind its first flight when the flight's first round acknowledged less than this
/// fraction of what was due (ADR-10156): the bytes sent, less those of the last round trip. The
/// soak's 256 kbit/s link acknowledges under half of a floor-rate flight; a path that carries it
/// acknowledges all of it but its losses.
pub const FIRST_FLIGHT_BEHIND: f64 = 0.75;
/// Rounds the round-trip delay gradient is read over.
pub const QUEUE_TREND_ROUNDS: usize = 4;
/// Queueing delay growth, milliseconds per millisecond, that means the sender is over the path's
/// rate (2 %).
pub const QUEUE_TREND_SLOPE: f64 = 0.02;
/// The gradient is judged only once the queue holds this much, milliseconds.
pub const QUEUE_TREND_MIN_MS: f64 = 5.0;
/// Per-round decay of the packet-weighted loss estimate.
pub const LOSS_DECAY: f64 = 0.8;
/// Groups the trendline filter regresses over (GCC's window of 20 packet groups).
pub const TRENDLINE_GROUPS: usize = 20;
/// Gain of the delay-noise average per round (one eighth, as RFC 6298's RTTVAR uses a quarter
/// of its own; slower here because a round holds many samples).
pub const QUEUE_NOISE_GAIN: f64 = 0.125;
/// Delay noise the fixed thresholds already absorb, milliseconds: up to a tick of socket-read
/// phase on an endpoint that reads once per 33 ms tick. Noise above it raises the thresholds.
pub const QUEUE_NOISE_ALLOWANCE_MS: f64 = 10.0;
/// A gap without any acknowledgment this long (or three minimum round trips on a longer path)
/// is an outage: the controller restarts its signals and its startup (milliseconds).
pub const OUTAGE_RESTART_MS: u64 = 1_000;
/// The shared uplink's aggregate grows only while its peers deliver at least this fraction of
/// it (a validated rate, RFC 7661).
pub const UPLINK_VALIDATED: f64 = 0.95;
/// The fewest delivering peers that must see a queue over [`QUEUE_DELAY_THRESHOLD_MS`] in the same
/// round before the shared uplink reads it as the host's own (ADR-8733). A queue at the host's
/// uplink delays every peer behind it at once; a queue only one or a few peers see is their own
/// downlink, and their own controllers already answer it. With fewer peers than this the shared
/// uplink never reads a host queue: each peer's controller sees the same queue as the host would.
pub const UPLINK_SHARED_QUEUE_PEERS: usize = 4;
/// The window the acknowledged-rate bound reads the delivery rate over, milliseconds.
pub const DELIVERY_WINDOW_MS: u64 = 1_000;
/// Delivering peers that make a shared uplink's readings those of a crowd (ADR-10097). From this
/// many on, the median peer's queueing delay and loss are the host's own (the queues and the loss
/// of a few clients' own links are never the median of sixteen), so the uplink's measured rate is
/// kept through a hold, asked again by a probe, and first taken from the peers' loss. With fewer,
/// half of the peers seeing a queue at once is often their own links (congested/8 on a listen
/// server), and the aggregate keeps its earlier, cautious form: a hold at what the peers deliver
/// right now, the estimate dropped after [`BOTTLENECK_HOLD_MS`], no probe.
pub const UPLINK_CROWD_PEERS: usize = 16;
/// Updates in a row (one per [`MIN_ROUND_MS`]) in which the median loss of the delivering peers
/// must reach [`LOSS_CUT`] before a shared uplink without a target takes one (ADR-10097).
pub const UPLINK_LOSS_ROUNDS: u32 = 4;
/// How long a peer must have sent without an acknowledgment, while its own packets still arrive,
/// to count as starved by the host's uplink, milliseconds (ADR-10097): the most, and the wait
/// before the path's round trip is known. With a known round trip the wait is
/// [`UPLINK_STARVED_RTTS`] of its minimum (plus the acknowledgment delay), at least
/// [`UPLINK_STARVED_MIN_MS`] (ADR-10200; `Connection::starved_after_ms`).
pub const UPLINK_STARVED_MS: u64 = 1_000;
/// Minimum round trips without an acknowledgment that make a peer starved (ADR-10200). A full
/// drop-tail queue loses every packet of the peers flushed last; a whole second of that at the
/// start of a full listen server was a 2 s snapshot hole on a 20 ms path (listen-uplink/64,
/// seed 90, with the first flight on). Eight round trips is past any loss-recovery timer
/// (RFC 9002's probe timeout is one round trip and its variation), so one late acknowledgment
/// does not count.
pub const UPLINK_STARVED_RTTS: f64 = 8.0;
/// The shortest starvation wait, milliseconds (ADR-10200): on a LAN path eight round trips are a
/// few milliseconds, and a host's queue drains a peer's packets over a few ticks.
pub const UPLINK_STARVED_MIN_MS: u64 = 250;
/// Starved peers at once that give a crowded shared uplink without a target its first one
/// (ADR-10097): several, as for the queue ([`UPLINK_SHARED_QUEUE_PEERS`]); one or two clients
/// whose own downlinks died are not the host's uplink.
pub const UPLINK_STARVED_PEERS: usize = 4;
/// The span a shared uplink reads the rate its peers delivered over when it holds, cuts or first
/// takes a target, milliseconds (ADR-10097): one update's sum moves with every peer that joins
/// or leaves in it.
pub const UPLINK_CARRIED_MS: u64 = 250;
/// How far over its target a shared uplink sends while it probes for a faster uplink (ADR-10097).
pub const UPLINK_PROBE_GAIN: f64 = 0.125;
/// How long a shared uplink's probe lasts, milliseconds (ADR-10097). The probe ends at the first
/// update after it, at most [`MIN_ROUND_MS`] later, so gain times duration stays at or under
/// 2.5 % of a second: less than the headroom ([`BOTTLENECK_HEADROOM`]) the target keeps under
/// the uplink's rate, so no whole second with a probe in it carries more than the uplink does.
pub const UPLINK_PROBE_MS: u64 = 150;
/// How long after a probe a shared uplink still watches its peers' queueing delay for the
/// probe's queue, milliseconds (ADR-10097).
pub const UPLINK_PROBE_WATCH_MS: u64 = 400;
/// The rise of the peers' median queueing delay that shows a probe met the uplink's rate,
/// milliseconds (ADR-10097).
pub const UPLINK_PROBE_QUEUE_MS: f64 = 5.0;
/// How far over the estimate a shared uplink's peers must deliver during a probe that met no
/// queue for the estimate to rise to what they delivered (ADR-10097). A probe the peers did not
/// take that far says nothing about the uplink: adding a fixed step for every probe without a
/// queue walked the estimate over the uplink's rate (listen-uplink/64, seed 7: one probe, 6 % up,
/// a second at 101.1 %).
pub const UPLINK_PROBE_STEP: f64 = 0.02;
/// How soon the next probe follows one that met no queue, milliseconds (ADR-10097).
pub const UPLINK_PROBE_RETRY_MS: u64 = 1_000;
/// The share of its target a shared uplink's peers must deliver before it probes (ADR-10097): a
/// probe of an aggregate the peers leave unused would say nothing.
pub const UPLINK_PROBE_FILL: f64 = 0.85;
/// Rounds a loss probe waits before it judges whether the loss followed the rate down (the loss
/// estimate's memory is about five rounds).
pub const LOSS_PROBE_ROUNDS: u32 = 6;
/// Packets a loss probe resolves at least before it judges (a slow sender's probe runs longer,
/// up to four times [`LOSS_PROBE_ROUNDS`]).
pub const LOSS_PROBE_PACKETS: f64 = 20.0;
/// After a probe showed the loss does not follow the rate (random loss), loss without a queue is
/// left to redundancy this long before the next probe, milliseconds.
pub const RANDOM_LOSS_HOLD_MS: u64 = 10_000;
/// Per-round decay of the long-run loss estimate (about fifty rounds of memory, ADR-10157): the
/// path's own loss before a loss cut, read from rounds the cut's trigger did not select.
pub const LOSS_LONG_DECAY: f64 = 0.98;
/// Standard deviations of the loss memory's count over what a known loss level gives before the
/// smoothed loss counts as more than that level (ADR-10157): a sender of a few packets per round
/// reads 10 % on a path that loses 5 % every few seconds by chance alone.
pub const LOSS_SPIKE_SIGMAS: f64 = 3.0;
/// The log-likelihood ratio at which a loss probe's sequential test rules, `ln(19)`: Wald's
/// bounds for 5 % error either way (ADR-10157).
pub const LOSS_PROBE_CONFIDENCE: f64 = 2.944_438_979_166_44;
/// A loss probe tests "the loss stayed at its reference" against "it fell to this fraction of
/// it" (ADR-10157): the test is indifferent near half the reference, the former rule's line.
pub const LOSS_PROBE_FALLEN: f64 = 0.25;
/// Resolved packets (acknowledged or lost) the delivery rate of a loss cut without a queue spans
/// at least (ADR-10157): one round of a few packets delivers what its own losses left, and a cut
/// to it took 40 % for two lost packets of five.
const LOSS_EVIDENCE_PACKETS: u32 = 32;

/// Settings of one connection's rate controller (`config/net.ron` `rate_control`, format v4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateConfig {
    /// `false` turns the controller and the pacer off: every due packet leaves at once (the v0.5
    /// behavior, for tests of the raw transport and diagnosis).
    pub enabled: bool,
    /// Bytes per second the essential class (inputs, tick sync, lockstep frames, acknowledgments,
    /// keepalives) may always send, whatever the controller says (item 4). Endpoints default it to
    /// 16 KiB/s up (client) and 32 KiB/s down (server).
    pub floor_bytes_per_second: u32,
    /// Initial rate before the startup doubles it, bytes per second (item 6; at most 256 KiB/s).
    pub start_bytes_per_second: u32,
    /// The project's rate for this peer, bytes per second: the controller never targets more.
    /// 0 = no project cap.
    pub max_bytes_per_second: u32,
    /// The path carried less than the floor with loss over 50 % for this long: the session moves
    /// to `Interrupted` instead of flooding (item 4). Milliseconds.
    pub circuit_breaker_ms: u64,
    /// The first flight (ADR-10156; opt-in, off by default in v0.7 by the lead's revised ruling,
    /// ADR-10231; `config/net.ron` `session.rate_control.first_flight`, format v4). On: until
    /// the path has answered once (the first feedback round) the sender offers it the floor's
    /// rate and a first burst of [`START_BURST_MS`] of it, and a first round the sender filled
    /// starts the startup at what the path delivered in it instead of the start rate. `false`
    /// (the default): the first burst is the initial window at the start rate.
    #[serde(skip)]
    pub first_flight: bool,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            floor_bytes_per_second: 32 * 1024,
            start_bytes_per_second: 256 * 1024,
            max_bytes_per_second: 0,
            circuit_breaker_ms: 10_000,
            first_flight: false,
        }
    }
}

impl RateConfig {
    /// The client side's defaults: the floor is 16 KiB/s (upstream).
    pub fn client() -> Self {
        Self {
            floor_bytes_per_second: 16 * 1024,
            ..Self::default()
        }
    }

    /// The first target: `min(project rate, start rate, 256 KiB/s)` and at least the minimum.
    fn start_rate(&self) -> f64 {
        let mut start = self.start_bytes_per_second.min(256 * 1024);
        if self.max_bytes_per_second > 0 {
            start = start.min(self.max_bytes_per_second);
        }
        f64::from(start.max(MIN_TARGET_BYTES_PER_SECOND))
    }

    /// The rate of a first flight, bytes per second (ADR-10156): what the sender would send
    /// blind anyway, the essential floor; never more than the start rate, nor less than the
    /// minimum target.
    pub fn first_flight_bytes_per_second(&self) -> u32 {
        let floor = self.floor_bytes_per_second.max(MIN_TARGET_BYTES_PER_SECOND);
        (self.start_rate() as u32).min(floor)
    }

    /// The most a connection's first burst may carry, bytes (ADR-10145). A start rate at its
    /// maximum (256 KiB/s, the default) sets no bound: the initial window leaves as the link is
    /// configured (`usize::MAX`). A lowered start rate allows [`START_BURST_MS`] of itself, and
    /// a first flight ([`Self::first_flight`], ADR-10156) that much of the first flight's rate;
    /// either way at least [`PACING_BURST_PACKETS`] packets of `packet_bytes`.
    pub fn start_burst_bytes(&self, packet_bytes: usize) -> usize {
        let rate = if self.first_flight {
            self.first_flight_bytes_per_second()
        } else if self.start_bytes_per_second >= 256 * 1024 {
            return usize::MAX;
        } else {
            self.start_bytes_per_second
        };
        let burst = usize::try_from(u64::from(rate) * START_BURST_MS / 1_000).unwrap_or(usize::MAX);
        burst.max(PACING_BURST_PACKETS.saturating_mul(packet_bytes))
    }

    fn max_rate(&self) -> f64 {
        if self.max_bytes_per_second == 0 {
            f64::from(u32::MAX)
        } else {
            f64::from(self.max_bytes_per_second.max(MIN_TARGET_BYTES_PER_SECOND))
        }
    }
}

/// The `session.rate_control` section of `config/net.ron` (format v4, ADR-8301): the project's
/// rate-controller settings for both ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateSettings {
    /// `false` turns the controller and pacer off on both ends.
    pub enabled: bool,
    /// The client's essential floor, bytes per second (default 16 KiB/s).
    pub floor_up_bytes_per_second: u32,
    /// The server's essential floor toward each client, bytes per second (default 32 KiB/s).
    pub floor_down_bytes_per_second: u32,
    /// The start rate, bytes per second (at most 256 KiB/s).
    pub start_bytes_per_second: u32,
    /// The circuit-breaker time, milliseconds (default 10 s).
    pub circuit_breaker_ms: u64,
    /// A listen server shares its host's uplink among its clients (default on).
    pub listen_shared_uplink: bool,
    /// A fixed shared uplink in bytes per second for any server (0 = measured, listen servers
    /// only).
    pub uplink_bytes_per_second: u32,
    /// The first flight on both ends (ADR-10156; opt-in, default off in v0.7, ADR-10231; format
    /// v4): on, until a path has answered once its sender offers it the essential floor's rate
    /// and eight packets, and the startup begins at what the first round delivered. `false`
    /// (the default) sends the 64-packet initial window at the start rate before the path has
    /// answered (v3 behavior).
    pub first_flight: bool,
}

impl Default for RateSettings {
    fn default() -> Self {
        let server = RateConfig::default();
        Self {
            enabled: server.enabled,
            floor_up_bytes_per_second: RateConfig::client().floor_bytes_per_second,
            floor_down_bytes_per_second: server.floor_bytes_per_second,
            start_bytes_per_second: server.start_bytes_per_second,
            circuit_breaker_ms: server.circuit_breaker_ms,
            listen_shared_uplink: true,
            uplink_bytes_per_second: 0,
            first_flight: server.first_flight,
        }
    }
}

impl RateSettings {
    /// The server end's controller settings.
    pub fn server(&self) -> RateConfig {
        RateConfig {
            enabled: self.enabled,
            floor_bytes_per_second: self.floor_down_bytes_per_second,
            start_bytes_per_second: self.start_bytes_per_second,
            max_bytes_per_second: 0,
            circuit_breaker_ms: self.circuit_breaker_ms,
            first_flight: self.first_flight,
        }
    }

    /// The client end's controller settings.
    pub fn client(&self) -> RateConfig {
        RateConfig {
            floor_bytes_per_second: self.floor_up_bytes_per_second,
            ..self.server()
        }
    }
}

/// What one feedback round measured (spec 120 § 4.2 item 1).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RoundSample {
    /// How long the round lasted, milliseconds.
    pub interval_ms: u64,
    /// Bytes newly acknowledged in the round (every packet, headers excluded).
    pub acked_bytes: u64,
    /// Bytes sent in the round.
    pub sent_bytes: u64,
    /// Packets newly acknowledged in the round.
    pub acked_packets: u32,
    /// Packets declared lost in the round (spurious losses taken back).
    pub lost_packets: u32,
    /// The smallest round-trip sample of the round, milliseconds (0 = none). Queueing delay is
    /// read from it, so per-packet jitter does not look like a queue: a queue delays every packet
    /// of the round.
    pub round_min_rtt_ms: f64,
    /// The smoothed round trip, milliseconds (0 = no sample yet).
    pub srtt_ms: f64,
    /// The minimum round trip of the last 10 s, milliseconds (0 = no sample yet).
    pub min_rtt_ms: f64,
    /// Delay samples the round took (round trips, and one-way delays with them): one for each
    /// acknowledgment frame that newly named this side's newest packet.
    pub delay_samples: u32,
    /// The smallest one-way delay sample of the round on [`OneWayDelay`]'s scale, milliseconds
    /// (0 = none): this side's packets only, the return path left out (wire 6, ADR-10050).
    pub round_min_one_way_ms: f64,
    /// The minimum one-way delay of the last [`ONE_WAY_WINDOW_MS`] on the same scale,
    /// milliseconds (0 = no sample yet).
    pub min_one_way_ms: f64,
    /// The trendline filter's verdict when receive timestamps are negotiated.
    pub gradient: Option<GradientSignal>,
    /// Bytes per second the endpoint offered the controller's pacer in the round beyond what it
    /// let through (the sender wanted more): not app-limited. Any nonzero value marks the round
    /// as filled (the transport passes the polls the pacer held data back in, the essential class
    /// included), as does sending 80 % of the target; only filled rounds grow a startup or count
    /// toward its full-pipe exit (ADR-8592).
    pub held_bytes: u64,
    /// A shared uplink's fair share was below this controller's target in the round: the
    /// aggregate controller owns the rate (spec 120 § 4.2 item 8).
    pub capped: bool,
    /// Bytes of the essential class (and of acknowledgment-only packets) sent in the round: what
    /// leaves inside the floor whatever the target says (item 4). The target never falls under
    /// their rate plus [`ESSENTIAL_RESERVE_BYTES_PER_SECOND`] (ADR-8534).
    pub essential_bytes: u64,
}

/// The delay-gradient detector's verdict (GCC's over-use detector).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GradientSignal {
    /// The one-way delay grows faster than the adaptive threshold.
    Overuse,
    /// The one-way delay shrinks faster than the threshold.
    Underuse,
    /// Neither.
    Normal,
}

/// The controller's state (GCC's remote-rate state machine plus BBR's bounded startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RateState {
    /// Doubling per round while the delivery rate keeps up (item 6).
    #[default]
    Startup,
    /// Growing: multiplicative far from the last cut, additive near it.
    Increase,
    /// Holding: the queue drains, or loss is between 2 % and 10 %.
    Hold,
    /// The latest round cut the rate.
    Decrease,
}

impl RateState {
    /// `startup`, `increase`, `hold` or `decrease`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Increase => "increase",
            Self::Hold => "hold",
            Self::Decrease => "decrease",
        }
    }
}

/// A loss cut without a queue, waiting to see whether the loss follows the rate down (a policer
/// or shallow buffer) or stays (random loss, F7).
#[derive(Debug, Clone, Copy, PartialEq)]
struct LossProbe {
    /// The target and state before the cut.
    pre: f64,
    pre_state: RateState,
    /// The loss the path showed before the cut, which the loss after it is compared with: the
    /// smoothed loss that triggered the cut when it is more than the path's long-run loss
    /// explains, else the long-run loss (ADR-10157: the trigger is the highest of many readings,
    /// so on a path that loses 5 % at random it reads 10-13 %, and half of that is the path's
    /// own loss).
    reference: f64,
    /// The sequential test's log-likelihood ratio: positive, the loss stayed (random loss);
    /// negative, it fell with the rate.
    llr: f64,
    at_ms: u64,
    rounds: u32,
    /// Packets lost and resolved since the cut took effect (from the second round on: the first
    /// acknowledges packets sent before the cut).
    lost: f64,
    resolved: f64,
}

/// A loss-driven cut that the controller owes back (item 5).
#[derive(Debug, Clone, Copy, PartialEq)]
struct LossCut {
    /// The rate before the first cut of the episode.
    pre_cut: f64,
    /// The rate when the loss cleared, and when.
    cleared: Option<(f64, u64)>,
    /// The rate the path delivered at the episode's first cut with a queue at its bottleneck (a
    /// congestion cut): the pre-cut rate is the one that overflowed it (ADR-8534).
    path_rate: Option<f64>,
    /// Rounds the recovery spent at the headroom under `path_rate` with no queue (ADR-8534).
    clean: u32,
}

/// A standing queue this sender built at the path's bottleneck, being drained (ADR-10145).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drain {
    /// The target while the drain lasts, bytes per second.
    rate: f64,
    /// The bottleneck's delivery rate the drain was scaled from, bytes per second.
    bottleneck: f64,
    /// When it ends at the latest.
    until_ms: u64,
    /// The queue it started from, milliseconds.
    queue_ms: f64,
}

/// Per-peer send-rate controller (spec 120 § 4.2). Pure: fed acks and time, asked for an allowance.
#[derive(Debug, Clone)]
pub struct RateController {
    config: RateConfig,
    target: f64,
    state: RateState,
    /// Queueing delay of the previous round and the rounds it has been rising above the threshold.
    prev_queue_ms: f64,
    rising_rounds: u32,
    /// Packet-weighted loss with a half-life of one round: lost, resolved.
    loss_w: (f64, f64),
    /// The delivery rate of the latest round, bytes per second.
    delivery: f64,
    /// The same loss with [`LOSS_LONG_DECAY`], restarted when a loss cut without a queue takes
    /// effect: the path's long-run loss at a rate it carries (ADR-10157).
    loss_long: (f64, f64),
    /// The part of `loss_w` from before the latest loss cut without a queue took effect, decaying
    /// with it: that loss was answered by the cut, and another cut needs loss since (ADR-10157).
    loss_stale: (f64, f64),
    /// (end, length, bytes acknowledged, packets resolved) of the rounds of the latest
    /// [`DELIVERY_WINDOW_MS`]: the app-limited bound reads their delivery rate (an acknowledged
    /// rate, as GCC's bound does).
    deliveries: std::collections::VecDeque<(u64, u64, u64, u32)>,
    /// The send rate of the previous round: a round's acknowledgments are of packets sent about
    /// one round earlier, so the startup compares the delivery rate with it.
    prev_sent_rate: f64,
    /// The previous round sent 80 % of its target or more: its packets are the ones this round
    /// acknowledges (ADR-10157).
    prev_saturating: bool,
    loss_probe: Option<LossProbe>,
    /// Until when loss without a queue is known to be random (a probe showed it does not follow
    /// the rate).
    random_loss_until: Option<u64>,
    /// The rate the latest delay-driven decrease cut from: increases near it are additive.
    last_cut_rate: Option<f64>,
    last_decrease_ms: Option<u64>,
    loss_cut: Option<LossCut>,
    /// Startup: the best delivery rate so far and rounds without 25 % growth.
    startup_best: f64,
    startup_flat_rounds: u32,
    /// When the path began failing to carry the floor at over 50 % loss.
    breaker_since: Option<u64>,
    breaker_tripped: bool,
    /// Queueing delay of the latest round, milliseconds.
    queue_ms: f64,
    /// (time, queueing delay) of the latest [`QUEUE_TREND_ROUNDS`] rounds: the round-trip delay
    /// gradient when receive timestamps are not negotiated.
    queue_trend: std::collections::VecDeque<(f64, f64)>,
    /// The smallest round-trip sample and the packets acknowledged of each of the latest rounds
    /// with a sample (at most `QUEUE_EVIDENCE_MAX_ROUNDS`).
    rtt_floor: std::collections::VecDeque<(f64, u32)>,
    /// The smallest one-way delay sample and the delay samples of each of the latest rounds with
    /// one (at most `QUEUE_EVIDENCE_MAX_ROUNDS`; ADR-10050).
    one_way_floor: std::collections::VecDeque<(f64, u32)>,
    /// Rounds in which an app-limited sender's overuse verdict waited for its one-way delay.
    unconfirmed: u64,
    rounds: u64,
    cuts: u64,
    /// The latest round that acknowledged a packet.
    last_acked_ms: Option<u64>,
    /// The smoothed delay noise of the round queue readings, milliseconds.
    queue_noise_ms: f64,
    /// The bottleneck estimate (bytes per second) and when a cut with queue evidence measured it
    /// (ADR-8490): growth stops at [`BOTTLENECK_HEADROOM`] of it for [`BOTTLENECK_HOLD_MS`].
    bottleneck: Option<(f64, u64)>,
    /// Until when, after an outage, newly declared losses are the outage's.
    outage_grace_until: Option<u64>,
    outages: u64,
    /// The essential class's send rate, bytes per second, smoothed over rounds (ADR-8534).
    essential_rate: f64,
    /// A shared uplink's aggregate controller splits this connection's rate (item 8): an overuse
    /// cut reads the round alone there (ADR-8859).
    shared_uplink: bool,
    /// Since when the delivery window has carried data (at least the floor's rate), restarted by
    /// an idle window or an outage: an unfilled round ends a startup only after a whole
    /// [`DELIVERY_WINDOW_MS`] of it (ADR-8592).
    data_since_ms: Option<u64>,
    /// The drain of a standing queue in force (ADR-10145): while it lasts the controller answers
    /// with the drain's rate, and `target` is the rate the sender returns to.
    drain: Option<Drain>,
    /// Until when no drain starts: the latest one did not move the queue.
    drain_refused_until: Option<u64>,
    /// When the latest drain ended with its queue gone.
    drain_done_ms: Option<u64>,
    /// Until when the bottleneck is confirmed: a second deep queue stood within
    /// [`BOTTLENECK_HOLD_MS`] of a drain that emptied the first. A loss cut's recovery then stops
    /// at the headroom (ADR-10145).
    bottleneck_confirmed_until: Option<u64>,
    drains: u64,
    /// The first flight is out and no feedback round has acknowledged a packet yet (ADR-10156):
    /// the sender is held to the first flight's rate. Never set when the config's `first_flight`
    /// is off (the default in v0.7, ADR-10231).
    first_flight: bool,
    /// The delivery readings up to this round's time were of packets sent under the first flight
    /// (the round that ended it and the next one, ADR-10200): the app-limited bound does not read
    /// them as what the game uses.
    flight_limited_until_ms: Option<u64>,
    /// Rounds still to come whose acknowledged packets left under the first flight.
    flight_rounds_left: u8,
}

impl RateController {
    /// A controller at its start rate.
    pub fn new(config: RateConfig) -> Self {
        let target = config.start_rate();
        let first_flight = config.first_flight;
        Self {
            config,
            target,
            state: RateState::Startup,
            prev_queue_ms: 0.0,
            rising_rounds: 0,
            loss_w: (0.0, 0.0),
            loss_long: (0.0, 0.0),
            loss_stale: (0.0, 0.0),
            delivery: 0.0,
            last_cut_rate: None,
            last_decrease_ms: None,
            loss_cut: None,
            startup_best: 0.0,
            startup_flat_rounds: 0,
            deliveries: std::collections::VecDeque::new(),
            prev_sent_rate: 0.0,
            prev_saturating: false,
            loss_probe: None,
            random_loss_until: None,
            breaker_since: None,
            breaker_tripped: false,
            queue_ms: 0.0,
            queue_trend: std::collections::VecDeque::new(),
            rtt_floor: std::collections::VecDeque::new(),
            one_way_floor: std::collections::VecDeque::new(),
            unconfirmed: 0,
            rounds: 0,
            cuts: 0,
            last_acked_ms: None,
            queue_noise_ms: 0.0,
            bottleneck: None,
            outage_grace_until: None,
            outages: 0,
            essential_rate: 0.0,
            shared_uplink: false,
            data_since_ms: None,
            drain: None,
            drain_refused_until: None,
            drain_done_ms: None,
            bottleneck_confirmed_until: None,
            drains: 0,
            first_flight,
            flight_limited_until_ms: None,
            flight_rounds_left: 0,
        }
    }

    /// The settings.
    pub fn config(&self) -> &RateConfig {
        &self.config
    }

    /// Tells the controller whether its sender splits a shared uplink among its peers (spec 120
    /// § 4.2 item 8; a server with a `SharedUplink` marks every peer, whether or not a share is in
    /// force). There an overuse cut keeps reading the round's own delivery rate: the aggregate
    /// hands out the whole uplink, and the peers' deep cuts are part of what keeps the sum under
    /// it and under the peers' own links (with the latest second's rate, listen-uplink/64 seed 3
    /// offers the host's uplink 100.04 % for two seconds, and congested/8 on a listen server has
    /// a second over a client's downlink on 20 of 40 seeds instead of 6; ADR-8859).
    pub fn set_shared_uplink(&mut self, shared: bool) {
        self.shared_uplink = shared;
    }

    /// Changes the project rate (the most the controller targets); the target is clamped to it.
    pub fn set_max_bytes_per_second(&mut self, max: u32) {
        self.config.max_bytes_per_second = max;
        self.target = self.target.min(self.config.max_rate());
    }

    /// Bytes per second the peer may send now, everything included. The essential class may use
    /// up to [`Self::floor_bytes_per_second`] even when this is lower.
    pub fn target_bytes_per_second(&self) -> u32 {
        self.target_in_force().clamp(0.0, f64::from(u32::MAX)) as u32
    }

    /// The target the sender is held to now: the drain's rate while a standing queue is drained
    /// (never under the minimum target or the essential class's reserve, ADR-8534), the first
    /// flight's rate until the path has answered (ADR-10156), the controller's target otherwise.
    fn target_in_force(&self) -> f64 {
        let target = if self.first_flight {
            self.target
                .min(f64::from(self.config.first_flight_bytes_per_second()))
        } else {
            self.target
        };
        match self.drain {
            Some(drain) => target.min(drain.rate.max(self.least_target())),
            None => target,
        }
    }

    /// True while the sender is held to its first flight (ADR-10156): `RateConfig::first_flight`
    /// is on and no feedback round has acknowledged a packet yet.
    pub fn first_flight(&self) -> bool {
        self.first_flight
    }

    /// The least the target ever is: the minimum target, or the essential class's own rate (up
    /// to the floor) plus its reserve (ADR-8534).
    fn least_target(&self) -> f64 {
        let essential_floor = self
            .essential_rate
            .min(f64::from(self.config.floor_bytes_per_second))
            + f64::from(ESSENTIAL_RESERVE_BYTES_PER_SECOND);
        f64::from(MIN_TARGET_BYTES_PER_SECOND).max(essential_floor)
    }

    /// True while the controller drains a standing queue (ADR-10145).
    pub fn draining(&self) -> bool {
        self.drain.is_some()
    }

    /// Drains started so far (ADR-10145); a drain renewed because the queue was still falling at
    /// its deadline counts once.
    pub fn drains(&self) -> u64 {
        self.drains
    }

    /// The essential class's floor, bytes per second.
    pub fn floor_bytes_per_second(&self) -> u32 {
        self.config.floor_bytes_per_second
    }

    /// Which state the controller is in, for quality signals and tests.
    pub fn state(&self) -> RateState {
        self.state
    }

    /// Queueing delay over the minimum round trip in the latest round, milliseconds.
    pub fn queue_delay_ms(&self) -> f64 {
        self.queue_ms
    }

    /// The latest round's delivery rate, bytes per second.
    pub fn delivery_bytes_per_second(&self) -> f64 {
        self.delivery
    }

    /// The smoothed loss fraction the loss bound reads.
    pub fn loss_fraction(&self) -> f64 {
        if self.loss_w.1 > 0.0 {
            self.loss_w.0 / self.loss_w.1
        } else {
            0.0
        }
    }

    /// Rounds fed so far.
    pub fn rounds(&self) -> u64 {
        self.rounds
    }

    /// Rate cuts so far (delay- or loss-driven).
    pub fn cuts(&self) -> u64 {
        self.cuts
    }

    /// Rounds in which an overuse verdict was held back because the sender was app-limited and
    /// its own packets' one-way delay showed no queue on the way out (ADR-10050). A verdict that
    /// waits several rounds counts in each of them.
    pub fn unconfirmed_overuse(&self) -> u64 {
        self.unconfirmed
    }

    /// Outages the controller restarted after (no acknowledgment for [`OUTAGE_RESTART_MS`] or
    /// longer).
    pub fn outages(&self) -> u64 {
        self.outages
    }

    /// True once the path failed to carry even the floor at over 50 % loss for
    /// `circuit_breaker_ms` (item 4); cleared by the first round that delivers the floor again.
    pub fn breaker_tripped(&self) -> bool {
        self.breaker_tripped
    }

    /// Feeds one ack round: newly acked bytes, the round trips and the round's loss.
    pub fn on_round(&mut self, now_ms: u64, sample: &RoundSample) {
        self.rounds += 1;
        // The target the round's packets left under (a drain's rate while one lasted).
        let offered_target = self.target_in_force();
        // The path has answered (the round acknowledged a packet): the first flight is over
        // (ADR-10156). A round that acknowledged nothing leaves it in force.
        let first_flight = self.first_flight && sample.acked_packets > 0;
        if first_flight {
            self.first_flight = false;
            // This round's packets and the next round's left under the flight (ADR-10200).
            self.flight_limited_until_ms = Some(now_ms);
            self.flight_rounds_left = 1;
        } else if self.flight_rounds_left > 0 {
            self.flight_rounds_left -= 1;
            self.flight_limited_until_ms = Some(now_ms);
        }
        let interval = sample.interval_ms.max(1) as f64;
        let delivery = sample.acked_bytes as f64 * 1000.0 / interval;
        let sent_rate = sample.sent_bytes as f64 * 1000.0 / interval;
        let essential = sample.essential_bytes as f64 * 1000.0 / interval;
        self.essential_rate += (essential - self.essential_rate) * ESSENTIAL_RATE_GAIN;
        self.delivery = delivery;
        self.deliveries.push_back((
            now_ms,
            sample.interval_ms.max(1),
            sample.acked_bytes,
            sample.acked_packets.saturating_add(sample.lost_packets),
        ));
        while self.deliveries.len() > 1
            && self
                .deliveries
                .front()
                .is_some_and(|d| now_ms.saturating_sub(d.0) >= DELIVERY_WINDOW_MS)
        {
            self.deliveries.pop_front();
        }
        // Since when the delivery window has carried data, not just keepalives and
        // acknowledgments (ADR-8592): an idle window restarts the measurement.
        if self.recent_delivery() >= f64::from(self.config.floor_bytes_per_second) {
            self.data_since_ms.get_or_insert(now_ms);
        } else {
            self.data_since_ms = None;
        }
        // Loss, packet-weighted, decaying by a fifth per round (about five rounds of memory: a
        // round of a few packets is too noisy to judge alone).
        // Packets the outage swallowed are declared lost in the round trips after it ends: they
        // say nothing about the rate (see the outage restart below).
        let lost_packets = if self.outage_grace_until.is_some_and(|t| now_ms < t) {
            0
        } else {
            sample.lost_packets
        };
        let resolved = f64::from(sample.acked_packets) + f64::from(lost_packets);
        self.loss_w = (
            self.loss_w.0 * LOSS_DECAY + f64::from(lost_packets),
            self.loss_w.1 * LOSS_DECAY + resolved,
        );
        self.loss_long = (
            self.loss_long.0 * LOSS_LONG_DECAY + f64::from(lost_packets),
            self.loss_long.1 * LOSS_LONG_DECAY + resolved,
        );
        self.loss_stale = (self.loss_stale.0 * LOSS_DECAY, self.loss_stale.1 * LOSS_DECAY);
        // An outage ended (no acknowledgment for OUTAGE_RESTART_MS, or three minimum round trips
        // on a long path): the round's round trips carry the outage and its losses were the
        // outage's, not the path's rate. The delay and loss history restart and so does the
        // startup, from the current target (RFC 9002 § 7.6's persistent congestion restarts the
        // window the same way; the rate is not cut, since an outage says nothing about it).
        let outage_gap = OUTAGE_RESTART_MS.max((3.0 * sample.min_rtt_ms.max(0.0)) as u64);
        let outage = sample.acked_packets > 0
            && self
                .last_acked_ms
                .is_some_and(|t| now_ms.saturating_sub(t) >= outage_gap);
        if sample.acked_packets > 0 {
            self.last_acked_ms = Some(now_ms);
        }
        if outage {
            // The round spans the outage: its delivery rate (and the window's mean) is not the
            // path's.
            self.deliveries.clear();
            self.data_since_ms = None;
            self.loss_w = (0.0, f64::from(sample.acked_packets));
            self.loss_long = self.loss_w;
            self.loss_stale = (0.0, 0.0);
            self.rtt_floor.clear();
            self.one_way_floor.clear();
            self.queue_trend.clear();
            self.prev_queue_ms = 0.0;
            self.rising_rounds = 0;
            self.queue_ms = 0.0;
            self.loss_probe = None;
            self.bottleneck = None;
            self.drain = None;
            self.drain_done_ms = None;
            self.bottleneck_confirmed_until = None;
            self.state = RateState::Startup;
            self.startup_best = 0.0;
            self.startup_flat_rounds = 0;
            self.prev_sent_rate = 0.0;
            self.prev_saturating = false;
            self.outages += 1;
            let rtt = sample.min_rtt_ms.max(MIN_ROUND_MS as f64);
            self.outage_grace_until = Some(now_ms.saturating_add((2.0 * rtt) as u64 + MIN_ROUND_MS));
            return;
        }
        let loss = self.loss_fraction();
        // Queueing delay from the smallest sample of the latest QUEUE_FLOOR_ROUNDS rounds: an
        // endpoint that reads its socket once per tick adds up to a tick to every sample, and a
        // round of one or two samples would read that phase as a queue. A real queue delays them
        // all.
        if sample.round_min_rtt_ms > 0.0 {
            if self.rtt_floor.len() >= QUEUE_EVIDENCE_MAX_ROUNDS {
                self.rtt_floor.pop_front();
            }
            self.rtt_floor
                .push_back((sample.round_min_rtt_ms, sample.acked_packets));
        }
        if sample.round_min_one_way_ms > 0.0 {
            if self.one_way_floor.len() >= QUEUE_EVIDENCE_MAX_ROUNDS {
                self.one_way_floor.pop_front();
            }
            self.one_way_floor
                .push_back((sample.round_min_one_way_ms, sample.delay_samples.max(1)));
        }
        let floor_queue = self.queue_over(QUEUE_FLOOR_ROUNDS, 0, sample.min_rtt_ms);
        // The threshold detector reads the round's own smallest sample: it reacts within the
        // round, and needs two rising rounds over 25 ms, which phase noise rarely gives.
        // While the path loses packets, the reading reaches back over the latest rounds until they
        // hold QUEUE_DETECTOR_LOSSY_PACKETS acknowledged packets (ADR-8490).
        let lossy = loss >= LOSS_IGNORE;
        let fresh_reading = sample.round_min_rtt_ms > 0.0 && sample.min_rtt_ms > 0.0;
        let raw_queue = if fresh_reading {
            (sample.round_min_rtt_ms - sample.min_rtt_ms).max(0.0)
        } else {
            self.queue_ms
        };
        let queue = if fresh_reading && lossy {
            self.queue_over(1, QUEUE_DETECTOR_LOSSY_PACKETS, sample.min_rtt_ms)
        } else {
            raw_queue
        };
        // The path's delay noise: how far a round's smallest sample sits over the floor of the
        // latest rounds (jitter, the phase of socket reads), smoothed. A real queue delays every
        // packet, so it moves the floor with it; the thresholds stand that much higher (GCC's
        // adaptive threshold idea, on round trips). While the reading reaches back, the noise is
        // the round's own reading over it: a queue delays the round's packets as much as the
        // earlier ones, jitter does not (ADR-8490).
        let spread = if lossy {
            raw_queue - queue
        } else {
            queue - floor_queue
        }
        .clamp(0.0, 4.0 * QUEUE_DELAY_THRESHOLD_MS);
        self.queue_noise_ms += (spread - self.queue_noise_ms) * QUEUE_NOISE_GAIN;
        let threshold =
            QUEUE_DELAY_THRESHOLD_MS + 2.0 * (self.queue_noise_ms - QUEUE_NOISE_ALLOWANCE_MS).max(0.0);
        let rising = queue > threshold && queue > self.prev_queue_ms;
        self.rising_rounds = if rising {
            self.rising_rounds + 1
        } else if queue > threshold {
            self.rising_rounds
        } else {
            0
        };
        let falling = queue + 2.0 < self.prev_queue_ms && queue > 5.0;
        self.prev_queue_ms = queue;
        self.queue_ms = queue;
        // The gradient's readings: the floor of the latest rounds, reaching back on a lossy path
        // until they hold QUEUE_DETECTOR_LOSSY_PACKETS acknowledged packets, as the threshold
        // detector's does (ADR-8490). At heavy loss a round acknowledges a handful of packets,
        // and the smallest of a few round trips on a jittery path climbs by the jitter alone: four
        // such rounds in a row read as a growing queue, the cut took one thin round's delivery,
        // and the bottleneck estimate measured at the cut rate then held the target for 15 s
        // (ADR-8658). A real queue delays every packet, so the reach-back still sees it grow.
        let trend_queue = if lossy {
            self.queue_over(
                QUEUE_FLOOR_ROUNDS,
                QUEUE_DETECTOR_LOSSY_PACKETS,
                sample.min_rtt_ms,
            )
        } else {
            floor_queue
        };
        if self.queue_trend.len() >= QUEUE_TREND_ROUNDS {
            self.queue_trend.pop_front();
        }
        self.queue_trend.push_back((now_ms as f64, trend_queue));
        let underuse = falling || sample.gradient == Some(GradientSignal::Underuse);
        // The sender used less than half its target: growth would only be a promise.
        let app_limited = sample.held_bytes == 0 && sent_rate < 0.5 * offered_target;
        // The sender fills its target: a delivery rate under it is the path's, not the game's.
        let saturating = sent_rate >= 0.8 * offered_target;
        // The round says something about the pipe: the sender wanted more than the pacer let
        // through (held), or it sent close to its target (a game that sizes its sends to the
        // allowance is never held) in this round and the one before (ADR-10157: one round can
        // hold a tick of sends more than its length, and two such rounds of a game at two thirds
        // of its target ended its startup at one round's delivery rate, a fifth over its demand,
        // where it then read as saturating for good). The only rounds the startup reads the pipe
        // from (ADR-8592).
        let filled = sample.held_bytes > 0 || (saturating && self.prev_saturating);
        // The round-trip delay gradient: a sender that pushes the path and sees the queue grow in
        // every one of the latest rounds faster than QUEUE_TREND_SLOPE is over the path's rate
        // before the queue reaches the threshold (GCC's gradient signal, read from round trips).
        let gradient_overuse = !app_limited
            && trend_queue >= QUEUE_TREND_MIN_MS
            && self.queue_trend.len() >= QUEUE_TREND_ROUNDS
            && self
                .queue_trend
                .iter()
                .zip(self.queue_trend.iter().skip(1))
                .all(|(a, b)| b.1 > a.1)
            && trend_slope(&self.queue_trend) > QUEUE_TREND_SLOPE;
        // A round without a round-trip sample of its own (at heavy loss most acknowledged packets
        // carried resent data, which Karn's rule does not sample) has no queue evidence: the floor
        // would re-read the latest rounds' readings, and a stale "queue" made every such round a
        // congestion cut (ADR-8400).
        let fresh = sample.round_min_rtt_ms > 0.0;
        // The queue a drain reads (ADR-10145): the round's own smallest sample over the minimum,
        // and no more than this side's packets met on the way out when the peer stamps arrivals
        // (the way back is not this sender's to drain).
        let one_way_now = (sample.round_min_one_way_ms > 0.0 && sample.min_one_way_ms > 0.0)
            .then(|| (sample.round_min_one_way_ms - sample.min_one_way_ms).max(0.0));
        let drain_queue = if fresh {
            one_way_now.map_or(raw_queue, |one_way| one_way.min(raw_queue))
        } else {
            0.0
        };
        // A startup ends on the first round whose every packet met a deep queue (ADR-10145;
        // the delay-increase exit of HyStart++, RFC 9406 § 4.2, with this controller's
        // thresholds): the start rate was a guess, and a queue of DRAIN_QUEUE_MS on the smallest
        // sample of a round is four standing-queue thresholds, where two rising rounds over the
        // threshold itself cost a round more of sending into it. The peers of a shared uplink
        // keep the two rounds: the queue they read may be the host's (ADR-8733).
        let startup_queue =
            self.state == RateState::Startup && !self.shared_uplink && drain_queue >= DRAIN_QUEUE_MS;
        let mut overuse = self.rising_rounds >= 2
            || gradient_overuse
            || sample.gradient == Some(GradientSignal::Overuse)
            || startup_queue;
        // A verdict from round trips needs the sender's own packets to confirm it when the sender
        // is app-limited (ADR-10050). A round trip carries the way back as well: the size of the
        // packet an acknowledgment rode in (36 ms between a 42-byte and a 1,200-byte packet at
        // 256 kbit/s), the queue the peer's own traffic stands in there, and the phase of two
        // endpoints' socket reads. None of them is a queue this sender built, and a sender that
        // uses less than half its target has no rate to give back for them: its cut only made
        // its next burst wait at its own pacer. The one-way delay of its packets (the peer's
        // arrival stamps, wire 6) shows the way out alone. Only an app-limited sender's verdicts
        // are checked: one that fills its target is read exactly as before (a saturating
        // sender's early cuts keep its seconds under a bottleneck's rate, and with every
        // verdict checked `congested` had a second over the link on 1 seed of 200 and
        // `rpc_p95_with_bulk` failed 2 of 104). The peers of a sender that splits a shared uplink
        // are read as before too: each uses a fraction of its own target while together they
        // fill the host's uplink, and their cuts are part of what holds the sum under it (with
        // them checked, listen-uplink/64 lost a client on 1 seed of 200). The trendline's
        // verdict is one-way already.
        // A startup's deep queue is read from this side's own packets already (above), and the
        // sender that built it is the one that filled the round before.
        if overuse
            && app_limited
            && !startup_queue
            && !self.shared_uplink
            && sample.gradient != Some(GradientSignal::Overuse)
        {
            let confirmed = self
                .one_way_queue(ONE_WAY_CONFIRM_SAMPLES, sample.min_one_way_ms)
                .is_none_or(|queue| queue >= ONE_WAY_CONFIRM_MS);
            if !confirmed {
                // The verdict waits: the rising count stays, so the cut follows in the round the
                // one-way delay confirms it. (Dropping it instead removed more of a client's cuts,
                // 46 a seed against 58, but a server is app-limited at times too, and its loss
                // cuts on the congested rows rose 5 %.)
                overuse = false;
                self.unconfirmed += 1;
            }
        }
        let standing = fresh && floor_queue > threshold;
        let srtt = sample.srtt_ms.max(1.0);
        // Queue evidence for the loss bound: a queue standing over the threshold on the floor of
        // the latest rounds, read back until they hold QUEUE_EVIDENCE_PACKETS acknowledged
        // packets. A real queue at a full drop-tail buffer delays every packet of those rounds;
        // the smallest of a few round trips on a jittery path sits over the minimum by the
        // jitter alone, and a slow sender that took it for congestion cut itself to the minimum
        // rate with every lossy round, where its rounds hold fewer packets still (ADR-8490).
        let queued = fresh
            && self.queue_over(QUEUE_FLOOR_ROUNDS, QUEUE_EVIDENCE_PACKETS, sample.min_rtt_ms) > threshold;
        // A delivery rate under the target is evidence against the path only when the packets
        // the round acknowledged were sent at about that target: the round before filled its
        // target too (ADR-10157). A game sends per tick and a round ends on an acknowledgment, so
        // a round that ends just after a tick holds one tick more than its length, and a sender
        // at two thirds of its target (where the app-limited bound below leaves it) read as
        // saturating for that round. Such a round ends just after a tick, so the next begins just
        // after one and holds no extra tick: no two rounds in a row read high. The first round of
        // a connection, or of a burst after an idle stretch, acknowledges packets of no filled
        // round either (a join's first round delivered 4 packets of a 53 kB burst before it
        // ended, and its 50 kB/s became the target).
        let filled_before = self.prev_saturating;
        // The round that ends a first flight filled the flight's rate, not the target the next
        // round is read against (ADR-10200): the packets the next round acknowledges left at the
        // floor's rate, and their delivery rate is no evidence against the path. Read as filled,
        // a lossy fat path's rejoin was cut to what its flight carried (lossy-fat-pipe/8: 63 to
        // 95 of 140 entities on 7 of 10 seeds, `random_loss_kept`).
        self.prev_saturating = saturating && !first_flight;
        // The loss since the latest loss cut without a queue took effect.
        let fresh_loss = self.fresh_loss_fraction();
        // A drain in force ends when a round's packets met almost no queue, or at its deadline.
        // At the deadline a queue that fell but still stands is drained again from its new
        // depth; one that did not fall is not this sender's, and is left alone for a while.
        // Before the deadline a deeper reading rescales the drain: the queue keeps growing for a
        // round trip after the cut (the packets already on their way), and the cut's own reading
        // was of packets sent a round trip earlier.
        if let Some(drain) = self.drain {
            if fresh && drain_queue < DRAIN_DONE_MS {
                self.drain = None;
                self.drain_done_ms = Some(now_ms);
            } else if now_ms >= drain.until_ms {
                self.drain = None;
                if fresh && drain_queue >= DRAIN_UNMOVED * drain.queue_ms {
                    self.drain_refused_until = Some(now_ms.saturating_add(DRAIN_RETRY_MS));
                } else if fresh && drain_queue >= DRAIN_QUEUE_MS {
                    self.drain = Some(Self::drain_of(now_ms, drain_queue, drain.bottleneck, srtt));
                }
            } else if fresh && drain_queue > drain.queue_ms {
                self.drain = Some(Self::drain_of(now_ms, drain_queue, drain.bottleneck, srtt));
            }
        }
        // The send rate whose packets this round acknowledged (the previous round's).
        let paced_rate = if self.prev_sent_rate > 0.0 {
            self.prev_sent_rate
        } else {
            sent_rate
        };
        self.prev_sent_rate = sent_rate;
        // A loss probe (item 3, F7): a cut without a queue is a question to the path. Loss that
        // falls once the rate is down came from the rate (a policer or a shallow buffer): the cut
        // stands. Loss that stays did not: the target goes back and loss without a queue is left
        // to redundancy for RANDOM_LOSS_HOLD_MS.
        let mut probing = false;
        if let Some(mut probe) = self.loss_probe {
            probe.rounds += 1;
            // The verdict is a sequential probability ratio test (Wald 1945) on the packets
            // resolved since the cut took effect: "the loss stayed at the reference" against "it
            // fell to LOSS_PROBE_FALLEN of it". Each lost packet and each acknowledged one adds its
            // log-likelihood ratio; the test rules at LOSS_PROBE_CONFIDENCE either way, so a thin
            // sender's probe runs until its packets can tell the two apart (the former rule
            // compared the loss of about 20 packets with half the trigger's reading, and on a
            // path losing 5 % at random it ruled "the loss followed the rate" two times in
            // three, ADR-10157).
            let stayed = probe.reference.clamp(1e-3, 0.9);
            let fallen = LOSS_PROBE_FALLEN * stayed;
            if probe.rounds == 1 {
                // The cut takes effect with the next round: the loss memory up to here is the
                // loss the cut answered.
                self.loss_stale = self.loss_w;
            } else if probe.rounds == 2 {
                // The path's long-run loss is its loss at a rate it carries: from here on.
                self.loss_long = (f64::from(lost_packets), resolved);
            }
            if probe.rounds >= 2 {
                probe.lost += f64::from(lost_packets);
                probe.resolved += resolved;
                probe.llr += f64::from(lost_packets) * (stayed / fallen).ln()
                    + f64::from(sample.acked_packets) * ((1.0 - stayed) / (1.0 - fallen)).ln();
            }
            let ruled = probe.llr.abs() >= LOSS_PROBE_CONFIDENCE && probe.resolved >= LOSS_PROBE_PACKETS;
            let judged = probe.rounds >= LOSS_PROBE_ROUNDS
                && now_ms.saturating_sub(probe.at_ms) as f64 >= 2.0 * srtt
                && (ruled || probe.rounds >= 4 * LOSS_PROBE_ROUNDS);
            if queued {
                // A queue owns the rate now: the cut was congestion.
                self.loss_probe = None;
            } else if judged {
                self.loss_probe = None;
                let probe_loss = probe.lost / probe.resolved.max(1.0);
                // A probe that ran out of rounds without a ruling falls back on the comparison
                // with half the reference.
                let random = if ruled {
                    probe.llr > 0.0
                } else {
                    probe_loss >= 0.5 * probe.reference
                };
                if random {
                    // The loss did not follow the rate down: random loss. The cut is given back
                    // (a startup resumes) and loss without a queue is left to redundancy, up to
                    // the level the path showed: loss well over it is judged again.
                    self.target = self.target.max(probe.pre);
                    if probe.pre_state == RateState::Startup {
                        self.state = RateState::Startup;
                    }
                    // An earlier cut of the same episode (the loss never cleared between them)
                    // stays owed: with the loss known to be the path's own it has cleared, and
                    // item 5 takes the target back to the episode's pre-cut rate. So a probe that
                    // ruled wrongly (one in twenty does) is undone by the next one (ADR-10157).
                    self.loss_cut = self.loss_cut.filter(|c| c.pre_cut > self.target);
                    self.random_loss_until = Some(now_ms.saturating_add(RANDOM_LOSS_HOLD_MS));
                    self.bottleneck = None;
                }
            } else {
                self.loss_probe = Some(probe);
                probing = true;
            }
        }
        // Known-random loss covers the path's long-run loss since the probe's cut took effect,
        // not any loss: loss since then that is well over it (a policer the restored rate ran
        // into, a path that got worse) is judged again.
        let random_loss = !queued
            && self.random_loss_until.is_some_and(|t| now_ms < t)
            && !self.fresh_loss_exceeds(self.long_loss_fraction());
        // The loss the bound judges: known-random loss without a queue is not congestion.
        let bound_loss = if random_loss { 0.0 } else { loss };

        // Circuit breaker (item 4).
        if loss > BREAKER_LOSS && delivery < f64::from(self.config.floor_bytes_per_second) {
            let since = *self.breaker_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= self.config.circuit_breaker_ms {
                self.breaker_tripped = true;
            }
        } else {
            self.breaker_since = None;
            if delivery >= f64::from(self.config.floor_bytes_per_second) || loss <= BREAKER_LOSS {
                self.breaker_tripped = false;
            }
        }

        let before = self.target;
        // A loss cut's recovery owed at the start of the round (item 5): the headroom below never
        // takes it back, not even in the round that completes it.
        let recovering = self.loss_cut.is_some();
        if overuse {
            // Item 2: at most one cut per round trip.
            if self
                .last_decrease_ms
                .is_none_or(|t| now_ms.saturating_sub(t) as f64 >= srtt)
            {
                // The path's delivery rate is read over the latest DELIVERY_WINDOW_MS (GCC cuts to
                // 0.85x the incoming rate of its 0.5-1 s window), never from this round alone when
                // the round is thinner: at a few packets per round trip one round's rate is
                // quantized by whole packets, and a round whose acknowledgments named one packet
                // took a target from 17.0 to 4.5 kB/s on a link carrying 16 (ADR-8859). A queue
                // that stood through the window delivered the bottleneck's rate over it. Under a
                // shared uplink the round alone decides, as before (`set_shared_uplink`).
                let delivered = if self.shared_uplink {
                    delivery
                } else {
                    delivery.max(self.recent_delivery())
                };
                let base = if delivered > 0.0 {
                    delivered.min(self.target)
                } else {
                    self.target
                };
                self.target = DECREASE_FACTOR * base;
                self.measure_bottleneck(now_ms);
                if (!app_limited || startup_queue) && !sample.capped {
                    // A startup's reading is a round trip old, and the sender has offered the
                    // path several times its rate since: the drain starts at its deepest.
                    let depth = if startup_queue {
                        drain_queue.max(DRAIN_HORIZON_MS * (1.0 - DRAIN_GAIN_MIN))
                    } else {
                        drain_queue
                    };
                    let _ = self.start_drain(now_ms, depth, base, srtt);
                }
                self.last_cut_rate = Some(before);
                self.last_decrease_ms = Some(now_ms);
                self.loss_cut = None;
                self.cuts += 1;
                self.rising_rounds = 0;
                // The queue keeps growing for one more round trip before the cut shows.
                self.queue_trend.clear();
            }
            self.state = RateState::Decrease;
        } else if probing {
            // A loss probe is out: no other decision until it is judged (a startup stays one).
            if self.state != RateState::Startup {
                self.state = RateState::Hold;
            }
        } else if bound_loss + 1e-9 >= LOSS_CUT
            && (queued
                || (saturating && filled_before && delivery < self.target && fresh_loss + 1e-9 >= LOSS_CUT))
        {
            // Item 3: loss with congestion evidence. A saturating sender also never targets more
            // than the round delivered (a full buffer or a policer passes no more). Without a
            // queue (a shallow buffer, a policer or random loss) the cut is a probe.
            let pre = self.loss_cut.map_or(before, |c| c.pre_cut);
            // Without a queue one loss event is one cut: the factor reads the loss since the
            // latest such cut took effect (ADR-10157; the memory of a join burst's loss cut the
            // rate again when its probe ended, with no packet lost in between).
            let cut_loss = if queued { loss } else { fresh_loss.min(loss) };
            self.target *= 1.0 - 0.5 * cut_loss;
            // Without a queue the delivery rate is read over LOSS_EVIDENCE_PACKETS resolved
            // packets: what one round of a few packets delivered is what its own losses left
            // (ADR-10157). A queue's cut keeps the round's own reading.
            let delivered = if queued {
                delivery
            } else {
                self.delivery_over(LOSS_EVIDENCE_PACKETS)
            };
            if saturating && !sample.capped && delivered > 0.0 {
                self.target = self.target.min(delivered);
            }
            if queued {
                self.measure_bottleneck(now_ms);
            } else {
                // The loss the probe compares with: the trigger's reading when it is more than
                // the path's long-run loss explains (the loss is new), else the long-run loss.
                let long = self.long_loss_fraction();
                let reference = if self.fresh_loss_exceeds(long) {
                    cut_loss
                } else {
                    long.min(cut_loss)
                };
                self.loss_probe = Some(LossProbe {
                    pre: before,
                    pre_state: self.state,
                    reference,
                    llr: 0.0,
                    at_ms: now_ms,
                    rounds: 0,
                    lost: 0.0,
                    resolved: 0.0,
                });
            }
            self.loss_cut = Some(LossCut {
                pre_cut: pre,
                cleared: None,
                path_rate: self
                    .loss_cut
                    .and_then(|c| c.path_rate)
                    .or_else(|| queued.then(|| self.recent_delivery()).filter(|r| *r > 0.0)),
                clean: 0,
            });
            self.cuts += 1;
            self.state = RateState::Decrease;
        } else if bound_loss >= LOSS_IGNORE && self.state != RateState::Startup {
            // Item 3: 2-10 % holds (or the loss is not congestive and redundancy answers it).
            // A startup goes on through loss without congestion evidence (below): it ends on a
            // flat delivery rate, a cut or a queue.
            if let Some(c) = self.loss_cut.as_mut() {
                c.cleared = None;
            }
            self.state = RateState::Hold;
        } else if let Some(mut cut) = self.loss_cut {
            // Item 5: back to the pre-cut rate within LOSS_RECOVERY_MS of the loss clearing.
            let (from, at) = *cut.cleared.get_or_insert((self.target, now_ms));
            let elapsed = now_ms.saturating_sub(at) as f64 + interval;
            let ratio = (cut.pre_cut / from.max(1.0)).max(1.0);
            let progress = (elapsed / LOSS_RECOVERY_MS as f64).min(1.0);
            self.target = self.target.max(from * ratio.powf(progress));
            // A congestion cut's pre-cut rate is the rate that overflowed the bottleneck: the
            // recovery owes it back only as far as BOTTLENECK_HEADROOM of the rate the path
            // delivered at the episode's first congestion cut. There, RECOVERY_CLEAN_ROUNDS rounds
            // sent at that rate with no queue show the path has room again (the loss was
            // transient, DO27 A4): the rest is owed at once. A queue there ends the recovery at
            // the headroom. Loss without a queue recovers in full, as before (ADR-8534).
            let owed = cut.path_rate.map_or(cut.pre_cut, |r| {
                cut.pre_cut.min((BOTTLENECK_HEADROOM * r).max(from))
            });
            self.target = self.target.min(owed.max(before));
            if self.target >= owed || progress >= 1.0 {
                self.target = self.target.max(owed);
                self.loss_cut = None;
                // The rest is not owed while a second deep queue has confirmed the bottleneck: a
                // drained queue no longer stands there to say the path is full (ADR-10145).
                let rest_owed =
                    owed < cut.pre_cut && self.bottleneck_confirmed_until.is_none_or(|t| now_ms >= t);
                if rest_owed && before >= owed && sent_rate >= 0.9 * owed && fresh {
                    if floor_queue < QUEUE_TREND_MIN_MS {
                        cut.clean += 1;
                        if cut.clean >= RECOVERY_CLEAN_ROUNDS {
                            self.target = cut.pre_cut;
                            self.bottleneck = None;
                        } else {
                            self.loss_cut = Some(cut);
                        }
                    }
                } else if rest_owed && now_ms.saturating_sub(at) < 2 * LOSS_RECOVERY_MS {
                    // Not sent at the headroom yet: wait a round or two for the evidence.
                    self.loss_cut = Some(cut);
                }
            } else {
                self.loss_cut = Some(cut);
            }
            self.state = RateState::Increase;
        } else if underuse || standing {
            // The queue drains, or stands above the threshold: no growth until it is gone.
            self.state = RateState::Hold;
        } else if self.state == RateState::Startup {
            // Item 6: double per round while the delivery rate keeps up; two rounds without 25 %
            // growth end the startup (BBR's full-pipe check, bounded). Only a round the sender
            // filled (the pacer held data back, the essential class included, or it sent 80 % of
            // the target) says anything about the pipe: any other round neither doubles nor
            // counts (BBR skips app-limited samples: a sender that left a fifth or more of its
            // target unused, with nothing held, is app-limited for that round), so a flat
            // app-limited sender is never drained to its own demand, and a startup that idles
            // through a handshake is still a startup (ADR-8592).
            // The delivery rate is compared with the rate of the packets it acknowledges (the
            // previous round's); what the round lost without congestion evidence (random loss:
            // with evidence the loss bound above cuts) does not count as a shortfall.
            if filled {
                // A round counts as flat only when its packets left at the best rate or above: a
                // sender still catching up with what the path once delivered (the initial window's
                // burst, or the rate before a probe cut) has not found the pipe's limit.
                if delivery > self.startup_best * 1.25 {
                    self.startup_best = delivery;
                    self.startup_flat_rounds = 0;
                } else if paced_rate >= self.startup_best {
                    self.startup_flat_rounds += 1;
                }
                let kept_up = delivery >= 0.8 * (1.0 - loss) * paced_rate;
                if self.startup_flat_rounds >= 2 {
                    // The full-pipe exit leaves the target at the best delivery rate (BBR's
                    // bottleneck estimate and its drain), not at the last doubled guess.
                    if self.startup_best > 0.0 {
                        self.target = self.target.min(self.startup_best.max(delivery));
                    }
                    self.state = RateState::Increase;
                } else if kept_up {
                    self.target *= 2.0;
                }
            } else if self
                .data_since_ms
                .is_some_and(|t| now_ms.saturating_sub(t) >= DELIVERY_WINDOW_MS)
            {
                // A round that left part of the target unused after a whole DELIVERY_WINDOW_MS of
                // data (RFC 7661's pipeACK sampling period of at least 1 s): the game's own rate is
                // measured and the startup is over. A later burst (a transfer) grows from there,
                // not by doubling into a deep buffer. Within the first second of data an unfilled
                // round only waits: a client whose first data round was half empty is not left at
                // 1.5x that round, under its demand (ADR-8592).
                self.state = RateState::Increase;
            }
        } else if !app_limited {
            let near = self
                .last_cut_rate
                .is_some_and(|c| self.target >= 0.7 * c && self.target <= 1.15 * c);
            if near {
                // One average packet per response time (GCC's additive increase): a slow link of
                // small packets grows by small steps.
                let response = srtt + 100.0;
                let packet = if sample.acked_packets > 0 {
                    (sample.acked_bytes as f64 / f64::from(sample.acked_packets)).clamp(100.0, 1_200.0)
                } else {
                    1_200.0
                };
                self.target += packet * (interval / response).min(1.0);
            } else {
                self.target *= (1.0 + INCREASE_PER_SECOND).powf(interval / 1000.0);
            }
            self.state = RateState::Increase;
        } else {
            // App-limited: no growth, but nothing holds it either.
            self.state = RateState::Increase;
        }
        // A deep queue that stands while this sender fills its target is drained (ADR-10145),
        // whichever signal cut the rate, or none: a loss cut leaves the target at the delivery
        // rate, which holds a full buffer full. The queue stood for the latest two rounds, the
        // delivery rate of the latest window is the bottleneck's, and the sender returns under it.
        if fresh
            && self.drain.is_none()
            && self.state != RateState::Startup
            && !app_limited
            && !sample.capped
            && floor_queue.min(drain_queue) >= DRAIN_QUEUE_MS
        {
            let delivered = delivery.max(self.recent_delivery());
            if delivered > 0.0 && self.start_drain(now_ms, drain_queue, delivered.min(self.target), srtt) {
                self.measure_bottleneck(now_ms);
                self.target = self.target.min(BOTTLENECK_HEADROOM * delivered);
            }
        }
        // Headroom under the bottleneck (ADR-8490): a saturating sender's growth stops at
        // BOTTLENECK_HEADROOM of the rate the path delivered when a queue last stood at its
        // bottleneck. Growth to the full rate found that rate by overshooting it: a
        // drop-tail buffer passes no more, the queue grows until the delay or the loss signal
        // returns, and the second that held the overshoot offered more than the link carried.
        // The estimate never cuts a target already over it, and a loss cut's recovery to its
        // pre-cut rate (item 5) is owed in full; it lapses after BOTTLENECK_HOLD_MS.
        if let Some((rate, at)) = self.bottleneck {
            if now_ms.saturating_sub(at) >= BOTTLENECK_HOLD_MS {
                self.bottleneck = None;
            } else if saturating
                && !recovering
                && self.loss_cut.is_none()
                && matches!(self.state, RateState::Increase | RateState::Startup)
            {
                self.target = self.target.min((BOTTLENECK_HEADROOM * rate).max(before));
            }
        }
        // An app-limited sender's target stays within 1.5x the delivery rate of the latest
        // DELIVERY_WINDOW_MS plus 10 kbit/s (GCC's acknowledged-rate bound): a stale target would
        // let a sudden burst blast the path before any signal returns. A loss cut owed back is
        // exempt. An idle sender (delivery under the essential floor: keepalives,
        // acknowledgments, inputs) keeps its target: what an idle link carried says nothing about
        // the path, and a connection that idled through a handshake or a loading screen is no
        // riskier than a fresh one at its start rate. The bound does not end a startup by itself
        // (ADR-8592): within the first second of data it lowers an unconfirmed start rate to what
        // the game uses, and the startup ends on the first unfilled round after that second.
        if app_limited && self.loss_cut.is_none() {
            // What the game uses, read after its first flight (ADR-10200): rounds of packets the
            // flight held to the floor's rate deliver the floor, not the game's demand, and a bound
            // at 1.5 times that brought a 52 kB/s game's target to 65 kB/s, where it read as
            // saturating and random loss cut it (ADR-10157's test, 6 of 32 seeds).
            let recent = self.recent_delivery_after(self.flight_limited_until_ms);
            let bound = 1.5 * recent + 1_250.0;
            if recent >= f64::from(self.config.floor_bytes_per_second) && self.target > bound {
                self.target = bound;
            }
        }
        // The essential class (item 4) leaves inside its floor whatever the target says: a target
        // under its rate lowers nothing that is sent and leaves the rest (the reliable stream's
        // repairs and catch-ups, replication; item 9) no allowance at all, so a lockstep stream or
        // a lost reliable message waits behind frames the controller cannot hold. The target keeps
        // ESSENTIAL_RESERVE_BYTES_PER_SECOND above that rate (up to the floor). A path that cannot
        // carry even the floor is the circuit breaker's (ADR-8534).
        // The first flight measured the path (ADR-10156). Its round was sent at the floor's rate,
        // so it can only say one thing: whether the path fell behind even that. What the flight
        // sent, less what its last round trip cannot have acknowledged yet, is due; a path that
        // acknowledged under FIRST_FLIGHT_BEHIND of it is slower than the flight, and the startup
        // begins at what it delivered (never under the flight's own rate): BBR starts the same
        // way, from the delivery rate of its initial window (Cardwell et al. 2016). A path that
        // kept up with the flight has shown no limit: the start rate stands, without the doubling
        // a round at the floor's rate would have earned it. A first round the sender did not fill
        // says nothing, as in any startup.
        if first_flight && self.state == RateState::Startup {
            self.target = self.target.min(self.config.start_rate());
            let unanswered =
                offered_target * PACING_GAIN * (sample.min_rtt_ms + QUEUE_DELAY_THRESHOLD_MS) / 1_000.0;
            let due = sample.sent_bytes as f64 - unanswered;
            if filled && due > 0.0 && (sample.acked_bytes as f64) < FIRST_FLIGHT_BEHIND * due {
                let flight = f64::from(self.config.first_flight_bytes_per_second());
                self.target = self.target.min(delivery.max(flight));
            }
        }
        self.target = self.target.max(self.least_target()).min(self.config.max_rate());
    }

    /// The essential class's send rate, bytes per second, smoothed over rounds: the target never
    /// falls under it plus [`ESSENTIAL_RESERVE_BYTES_PER_SECOND`] (ADR-8534).
    pub fn essential_bytes_per_second(&self) -> f64 {
        self.essential_rate
    }
}

impl RateController {
    /// The drain of a queue `queue_ms` deep at a bottleneck that delivers `bottleneck` bytes per
    /// second (ADR-10145): the deeper the queue, the lower the rate, for as long as the queue
    /// needs to empty at that rate plus one round for the signal to return.
    fn drain_of(now_ms: u64, queue_ms: f64, bottleneck: f64, srtt_ms: f64) -> Drain {
        let gain = (1.0 - queue_ms / DRAIN_HORIZON_MS).clamp(DRAIN_GAIN_MIN, DECREASE_FACTOR);
        let ms = (queue_ms / (1.0 - gain) + srtt_ms.max(MIN_ROUND_MS as f64)).ceil() as u64;
        Drain {
            rate: gain * bottleneck,
            bottleneck,
            until_ms: now_ms.saturating_add(ms.min(DRAIN_MAX_MS)),
            queue_ms,
        }
    }

    /// A congestion cut found `queue_ms` of queue standing at a bottleneck that delivers
    /// `bottleneck` bytes per second: a queue of [`DRAIN_QUEUE_MS`] or more is drained, unless a
    /// drain is in force already or the latest one did not move the queue (ADR-10145).
    ///
    /// A second deep queue within [`BOTTLENECK_HOLD_MS`] of a drain that emptied the first
    /// confirms the bottleneck: the sender filled it twice. True when a drain started.
    fn start_drain(&mut self, now_ms: u64, queue_ms: f64, bottleneck: f64, srtt_ms: f64) -> bool {
        if self.drain.is_some()
            || queue_ms < DRAIN_QUEUE_MS
            || bottleneck <= 0.0
            || self.drain_refused_until.is_some_and(|t| now_ms < t)
        {
            return false;
        }
        if self
            .drain_done_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < BOTTLENECK_HOLD_MS)
        {
            self.bottleneck_confirmed_until = Some(now_ms.saturating_add(BOTTLENECK_HOLD_MS));
        }
        self.drain = Some(Self::drain_of(now_ms, queue_ms, bottleneck, srtt_ms));
        self.drains += 1;
        true
    }

    /// A cut found a queue at the path's bottleneck: the link was busy, so the delivery rate of
    /// the latest [`DELIVERY_WINDOW_MS`] is its rate (a window that began before the queue reads
    /// lower, which errs on the safe side).
    fn measure_bottleneck(&mut self, now_ms: u64) {
        let rate = self.recent_delivery();
        if rate > 0.0 {
            self.bottleneck = Some((rate, now_ms));
        }
    }

    /// Queueing delay over `min_rtt_ms` on the smallest round-trip sample of the latest `rounds`
    /// rounds with a sample, reaching further back (up to `QUEUE_EVIDENCE_MAX_ROUNDS`) until they
    /// acknowledged at least `packets` packets; 0 without a sample.
    fn queue_over(&self, rounds: usize, packets: u32, min_rtt_ms: f64) -> f64 {
        let (mut floor, mut taken, mut held) = (f64::INFINITY, 0usize, 0u32);
        for &(rtt, acked) in self.rtt_floor.iter().rev() {
            if taken >= rounds && held >= packets {
                break;
            }
            floor = floor.min(rtt);
            taken += 1;
            held = held.saturating_add(acked);
        }
        if floor.is_finite() && min_rtt_ms > 0.0 {
            (floor - min_rtt_ms).max(0.0)
        } else {
            0.0
        }
    }

    /// The one-way queueing delay over `min_ms` on the smallest sample of the latest rounds that
    /// hold at least `samples` delay samples between them (at least `QUEUE_FLOOR_ROUNDS` rounds,
    /// at most `QUEUE_EVIDENCE_MAX_ROUNDS`); `None` without a one-way sample (a peer that sends
    /// no arrival stamp, or none since an outage).
    fn one_way_queue(&self, samples: u32, min_ms: f64) -> Option<f64> {
        let (mut floor, mut taken, mut held) = (f64::INFINITY, 0usize, 0u32);
        for &(delay, count) in self.one_way_floor.iter().rev() {
            if taken >= QUEUE_FLOOR_ROUNDS && held >= samples {
                break;
            }
            floor = floor.min(delay);
            taken += 1;
            held = held.saturating_add(count);
        }
        (floor.is_finite() && min_ms > 0.0).then(|| (floor - min_ms).max(0.0))
    }

    /// The delivery rate over the rounds of the latest [`DELIVERY_WINDOW_MS`], bytes per second.
    fn recent_delivery(&self) -> f64 {
        self.recent_delivery_after(None)
    }

    /// [`Self::recent_delivery`] over the rounds that ended after `after` (all of them for
    /// `None`), bytes per second; 0 when none did.
    fn recent_delivery_after(&self, after: Option<u64>) -> f64 {
        let (span, bytes) = self
            .deliveries
            .iter()
            .filter(|d| after.is_none_or(|t| d.0 > t))
            .fold((0u64, 0u64), |(t, b), d| (t + d.1, b + d.2));
        bytes as f64 * 1_000.0 / span.max(1) as f64
    }

    /// The delivery rate over the latest rounds of the delivery window, reaching back until they
    /// resolved at least `packets` packets (acknowledged or lost), bytes per second.
    fn delivery_over(&self, packets: u32) -> f64 {
        let (mut span, mut bytes, mut held) = (0u64, 0u64, 0u32);
        for &(_, length, acked, resolved) in self.deliveries.iter().rev() {
            span += length;
            bytes += acked;
            held = held.saturating_add(resolved);
            if held >= packets {
                break;
            }
        }
        bytes as f64 * 1_000.0 / span.max(1) as f64
    }

    /// The path's long-run loss fraction ([`LOSS_LONG_DECAY`]).
    fn long_loss_fraction(&self) -> f64 {
        if self.loss_long.1 > 0.0 {
            self.loss_long.0 / self.loss_long.1
        } else {
            0.0
        }
    }

    /// The loss memory without what predates the latest loss cut without a queue: lost, resolved.
    fn fresh_loss(&self) -> (f64, f64) {
        (
            (self.loss_w.0 - self.loss_stale.0).max(0.0),
            (self.loss_w.1 - self.loss_stale.1).max(0.0),
        )
    }

    /// The smoothed loss fraction since the latest loss cut without a queue took effect.
    fn fresh_loss_fraction(&self) -> f64 {
        let (lost, resolved) = self.fresh_loss();
        if resolved > 0.0 { lost / resolved } else { 0.0 }
    }

    /// True when the smoothed loss (since the latest loss cut without a queue took effect) is
    /// more than a path losing `level` of its packets explains: the loss memory's lost count is
    /// over its mean at that level by more than [`LOSS_SPIKE_SIGMAS`] standard deviations. The
    /// memory weights round `i` back by `LOSS_DECAY^i`, so with `n` its weighted packet count
    /// the count's variance is `n * level * (1 - level) / (1 + LOSS_DECAY)` (the sum of the
    /// squared weights over the sum of the weights is `1 / (1 + LOSS_DECAY)`).
    fn fresh_loss_exceeds(&self, level: f64) -> bool {
        let (lost, resolved) = self.fresh_loss();
        let level = level.clamp(0.0, 1.0);
        let deviation = (resolved * level * (1.0 - level) / (1.0 + LOSS_DECAY)).sqrt();
        lost > resolved * level + LOSS_SPIKE_SIGMAS * deviation
    }
}

/// The one-way delay of this side's packets, from the arrival stamps of the peer's
/// acknowledgment frames (`ack::AckFrame::arrival_ms`, wire 6; ADR-10050). The measurement of
/// RFC 6817 § 3.1 (LEDBAT): the receiver's clock at a packet's arrival minus the sender's clock
/// when it left. The two clocks share no origin, so only differences of samples mean anything:
/// a sample minus the windowed minimum is the queueing delay on the way out, whatever the return
/// path does. Samples are on a scale whose origin is this measurement's first sample
/// (always positive). Pure: time is passed in.
#[derive(Debug, Clone, Default)]
pub struct OneWayDelay {
    /// This side's clock at the latest stamp, and the peer's clock it named, unwrapped.
    last: Option<(u64, i64)>,
    /// What comes off a raw sample (peer clock minus this side's) to put it on the scale.
    origin: i64,
    /// Samples of the last [`ONE_WAY_WINDOW_MS`], as a monotonic queue of `(time, sample)` whose
    /// front is the minimum.
    window: std::collections::VecDeque<(u64, f64)>,
    latest: f64,
}

impl OneWayDelay {
    /// A measurement with no sample yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A packet sent at `sent_ms` (this side's clock) arrived when the peer's clock read
    /// `arrival` in its low 16 bits of milliseconds; the frame that says so arrived at `now_ms`.
    /// Returns the sample. The stamp is unwrapped against the peer's clock as estimated from the
    /// previous stamp (the peer's clock is assumed to run about as fast as this one, and the
    /// time an acknowledgment takes back is far under half the stamp's span); after
    /// [`ONE_WAY_RESTART_MS`] without a stamp the measurement restarts.
    pub fn sample(&mut self, now_ms: u64, sent_ms: u64, arrival: u16) -> f64 {
        let peer = match self.last {
            Some((at, peer)) if now_ms.saturating_sub(at) < ONE_WAY_RESTART_MS => {
                let expected = peer.saturating_add(now_ms.saturating_sub(at) as i64);
                expected.saturating_add(i64::from(arrival.wrapping_sub(expected as u16) as i16))
            }
            _ => {
                // The first stamp, or the first after a long silence: a new scale.
                self.window.clear();
                self.origin = i64::from(arrival).saturating_sub(sent_ms as i64);
                i64::from(arrival)
            }
        };
        self.last = Some((now_ms, peer));
        let raw = peer.saturating_sub(sent_ms as i64).saturating_sub(self.origin);
        let sample = (raw as f64 + ONE_WAY_ORIGIN_MS).max(1.0);
        while self
            .window
            .front()
            .is_some_and(|(at, _)| now_ms.saturating_sub(*at) > ONE_WAY_WINDOW_MS)
        {
            self.window.pop_front();
        }
        while self.window.back().is_some_and(|(_, m)| *m >= sample) {
            self.window.pop_back();
        }
        self.window.push_back((now_ms, sample));
        self.latest = sample;
        sample
    }

    /// The minimum of the last [`ONE_WAY_WINDOW_MS`] of samples, milliseconds on the
    /// measurement's scale (0 = no sample yet).
    pub fn min_ms(&self) -> f64 {
        self.window.front().map_or(0.0, |(_, m)| *m)
    }

    /// The latest sample over the minimum, milliseconds: the queueing delay the latest
    /// acknowledged packet met on the way out (0 without a sample).
    pub fn queue_ms(&self) -> f64 {
        (self.latest - self.min_ms()).max(0.0)
    }
}

/// Least-squares slope of `(x, y)` points.
fn trend_slope(points: &std::collections::VecDeque<(f64, f64)>) -> f64 {
    let n = points.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let mean_x = points.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.1).sum::<f64>() / n;
    let (mut num, mut den) = (0.0, 0.0);
    for (x, y) in points {
        num += (x - mean_x) * (y - mean_y);
        den += (x - mean_x) * (x - mean_x);
    }
    if den > 0.0 { num / den } else { 0.0 }
}

/// One packet group of the trendline filter: its send time and arrival time.
#[derive(Debug, Clone, Copy)]
struct Group {
    send_ms: f64,
    arrival_ms: f64,
}

/// GCC's arrival-time filter (draft-ietf-rmcat-gcc § 5.3 and its trendline estimator): packets
/// sent within 5 ms form a group, the inter-group delay variation is accumulated and smoothed, and
/// a least-squares slope over the last [`TRENDLINE_GROUPS`] groups is compared with an adaptive
/// threshold.
#[derive(Debug, Clone)]
pub struct TrendlineFilter {
    current: Option<Group>,
    previous: Option<Group>,
    accumulated: f64,
    smoothed: f64,
    points: std::collections::VecDeque<(f64, f64)>,
    threshold: f64,
    last_update_ms: Option<f64>,
    signal: GradientSignal,
    first_arrival: Option<f64>,
}

impl Default for TrendlineFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl TrendlineFilter {
    /// An empty filter.
    pub fn new() -> Self {
        Self {
            current: None,
            previous: None,
            accumulated: 0.0,
            smoothed: 0.0,
            points: std::collections::VecDeque::new(),
            threshold: 12.5,
            last_update_ms: None,
            signal: GradientSignal::Normal,
            first_arrival: None,
        }
    }

    /// Adds one packet: when this side sent it and when the peer received it (each side's own
    /// clock, milliseconds; only differences count).
    pub fn add(&mut self, send_ms: f64, arrival_ms: f64) {
        match self.current.as_mut() {
            Some(g) if send_ms - g.send_ms <= 5.0 && send_ms >= g.send_ms => {
                g.arrival_ms = g.arrival_ms.max(arrival_ms);
            }
            Some(_) => {
                let done = self.current.replace(Group { send_ms, arrival_ms });
                if let (Some(prev), Some(cur)) = (self.previous, done) {
                    self.update(prev, cur);
                }
                self.previous = done;
            }
            None => {
                self.current = Some(Group { send_ms, arrival_ms });
            }
        }
    }

    fn update(&mut self, prev: Group, cur: Group) {
        let variation = (cur.arrival_ms - prev.arrival_ms) - (cur.send_ms - prev.send_ms);
        self.accumulated += variation;
        self.smoothed = 0.9 * self.smoothed + 0.1 * self.accumulated;
        let base = *self.first_arrival.get_or_insert(cur.arrival_ms);
        self.points.push_back((cur.arrival_ms - base, self.smoothed));
        if self.points.len() > TRENDLINE_GROUPS {
            self.points.pop_front();
        }
        if self.points.len() < TRENDLINE_GROUPS {
            return;
        }
        let n = self.points.len() as f64;
        let mean_x = self.points.iter().map(|p| p.0).sum::<f64>() / n;
        let mean_y = self.points.iter().map(|p| p.1).sum::<f64>() / n;
        let (mut num, mut den) = (0.0, 0.0);
        for (x, y) in &self.points {
            num += (x - mean_x) * (y - mean_y);
            den += (x - mean_x) * (x - mean_x);
        }
        let slope = if den > 0.0 { num / den } else { 0.0 };
        // GCC's modified trend: the slope scaled by the group count and a gain of 4.
        let trend = slope * n.min(60.0) * 4.0;
        let now = cur.arrival_ms;
        let dt = self.last_update_ms.map_or(0.0, |t| (now - t).clamp(0.0, 100.0));
        self.last_update_ms = Some(now);
        // The adaptive threshold (k_up 0.0087, k_down 0.039 per ms), skipped for outliers.
        if (trend.abs() - self.threshold) <= 15.0 {
            let k = if trend.abs() < self.threshold {
                0.039
            } else {
                0.0087
            };
            self.threshold += k * (trend.abs() - self.threshold) * dt;
            self.threshold = self.threshold.clamp(6.0, 600.0);
        }
        self.signal = if trend > self.threshold {
            GradientSignal::Overuse
        } else if trend < -self.threshold {
            GradientSignal::Underuse
        } else {
            GradientSignal::Normal
        };
    }

    /// The latest verdict (`Normal` until twenty groups were seen).
    pub fn signal(&self) -> GradientSignal {
        self.signal
    }
}

/// A token bucket that spreads datagrams at a gain times the target: [`PACING_GAIN`] for the pacer
/// (spec 120 § 4.2 item 7, RFC 9002 § 7.7), 1.0 for the budget that holds a saturating sender's
/// average to the target (the scheduler's share of item 9).
#[derive(Debug, Clone)]
pub struct Pacer {
    tokens: f64,
    last_ms: Option<u64>,
    gain: f64,
    /// The tokens right after the latest refill: what the sender spent since is the difference.
    refilled: f64,
}

impl Pacer {
    /// A pacer at [`PACING_GAIN`] whose first burst may carry `initial_bytes` (the initial
    /// window).
    pub fn new(initial_bytes: usize) -> Self {
        Self::with_gain(initial_bytes, PACING_GAIN)
    }

    /// A bucket at `gain` times the rate it is refilled at.
    pub fn with_gain(initial_bytes: usize, gain: f64) -> Self {
        Self {
            tokens: initial_bytes as f64,
            last_ms: None,
            gain,
            refilled: initial_bytes as f64,
        }
    }

    /// The bucket depth at `rate` bytes per second: `ticks` ticks of the gained rate, at least
    /// [`PACING_BURST_PACKETS`] packets.
    pub fn depth(&self, rate: f64, tick_ms: u64, ticks: u64, packet_bytes: usize) -> f64 {
        (rate * self.gain * (tick_ms * ticks) as f64 / 1000.0)
            .max((PACING_BURST_PACKETS * packet_bytes) as f64)
    }

    /// Adds the tokens of the time since the last refill at `rate` bytes per second (times the
    /// gain), up to `depth`; a first burst above the depth is kept until it is spent.
    pub fn refill(&mut self, now_ms: u64, rate: f64, depth: f64) {
        let _ = self.refill_overflow(now_ms, rate, depth);
    }

    /// [`Pacer::refill`], returning the tokens that did not fit: allowance the sender left unused
    /// (the excess a message's burst may draw on, ADR-8859). Never more than the interval earned
    /// beyond what the sender spent in it (ADR-10063): a first burst above the depth is startup
    /// allowance, not target left unused, so a sender that spends at least its target earns
    /// nothing while it drains that burst.
    pub fn refill_overflow(&mut self, now_ms: u64, rate: f64, depth: f64) -> f64 {
        let dt = self.last_ms.map_or(0, |t| now_ms.saturating_sub(t));
        self.last_ms = Some(now_ms);
        let earned = rate * self.gain * dt as f64 / 1000.0;
        let spent = (self.refilled - self.tokens).max(0.0);
        let overflow = if self.tokens >= depth {
            earned
        } else {
            let filled = self.tokens + earned;
            self.tokens = filled.min(depth);
            (filled - depth).max(0.0)
        };
        self.refilled = self.tokens;
        overflow.min((earned - spent).max(0.0))
    }

    /// Drops tokens above `depth`: the first burst's allowance is withdrawn (a shared uplink's
    /// fair share owns the rate, ADR-8534).
    pub fn limit(&mut self, depth: f64) {
        self.tokens = self.tokens.min(depth);
        self.refilled = self.refilled.min(depth);
    }

    /// Whether a packet may leave now (the bucket is not in debt).
    pub fn ready(&self) -> bool {
        self.tokens > 0.0
    }

    /// Whether the bucket holds `bytes` whole: a packet of that size would leave it out of debt
    /// (background data never borrows, ADR-8658).
    pub(crate) fn covers(&self, bytes: usize) -> bool {
        self.tokens >= bytes as f64
    }

    /// Charges a sent packet.
    pub fn spend(&mut self, bytes: usize) {
        self.tokens -= bytes as f64;
    }

    /// When the bucket is out of debt again at `rate` bytes per second; `now_ms` when it is now.
    pub fn next_send_at_ms(&self, now_ms: u64, rate: f64) -> u64 {
        if self.tokens > 0.0 {
            return now_ms;
        }
        let rate = (rate * self.gain).max(1.0);
        let wait = ((-self.tokens + 1.0) * 1000.0 / rate).ceil() as u64;
        self.last_ms.unwrap_or(now_ms).saturating_add(wait).max(now_ms)
    }
}

/// What a sender's packets cost on the wire for each byte of packet body (ADR-10097, KI-NT-032):
/// a tally of the ack-eliciting packets sent lately, each weighted by its age
/// ([`WIRE_SHARE_MEMORY_MS`]). A packet's cost on the wire is its body, the endpoint header and
/// [`DATAGRAM_OVERHEAD_BYTES`].
///
/// A peer's own controller steers packet bodies: the headers are a cost per packet that a rate
/// cut does not shrink (a game sends a packet per tick at any rate). A link that several peers
/// share, or that an operator states a rate for, carries the headers too, so a
/// [`SharedUplink`] is fed and read in bytes on the wire: each peer's target and delivery rate
/// times its [`Self::ratio`] go in, and its share divided by the ratio is its cap.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WireShare {
    wire: f64,
    body: f64,
    at_ms: u64,
}

impl WireShare {
    /// A packet with `body_len` body bytes left behind an endpoint header of `header_len` bytes.
    pub fn sent(&mut self, now_ms: u64, body_len: usize, header_len: usize) {
        let age = now_ms.saturating_sub(self.at_ms) as f64;
        let keep = WIRE_SHARE_MEMORY_MS / (WIRE_SHARE_MEMORY_MS + age);
        self.wire = self.wire * keep + (body_len + header_len + DATAGRAM_OVERHEAD_BYTES) as f64;
        self.body = self.body * keep + body_len as f64;
        self.at_ms = self.at_ms.max(now_ms);
    }

    /// Bytes on the wire per body byte, at least 1 (1 before the first packet).
    pub fn ratio(&self) -> f64 {
        if self.body >= 1.0 {
            (self.wire / self.body).max(1.0)
        } else {
            1.0
        }
    }
}

/// One peer's input to the [`SharedUplink`]. Rates are bytes per second on the wire (ADR-10097:
/// the peer's own figures times its [`WireShare::ratio`]), and so are the shares that come back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UplinkPeer {
    /// The peer's own controller target, bytes per second (its demand).
    pub target: f64,
    /// Its latest delivery rate, bytes per second.
    pub delivery: f64,
    /// Its latest queueing delay, milliseconds.
    pub queue_ms: f64,
    /// Its share weight (players 1.0, spectators lower).
    pub weight: f64,
    /// Its smoothed loss fraction, 0 to 1 ([`RateController::loss_fraction`]): the shared uplink
    /// reads the host's own drops from the median of its delivering peers (ADR-10097).
    pub loss: f64,
    /// True when the peer has sent for [`UPLINK_STARVED_MS`] without an acknowledgment while its
    /// own packets still arrive (`Connection::unanswered_ms`, ADR-10097).
    pub starved: bool,
}

/// A shared uplink's probe for a faster uplink (ADR-10097).
#[derive(Debug, Clone, Copy)]
struct UplinkProbe {
    /// The boost ends at the first update at or after this.
    until_ms: u64,
    /// The verdict falls at the first update at or after this.
    watch_until_ms: u64,
    /// The peers' median queueing delay (the floor of the latest updates) when the probe began.
    baseline_ms: f64,
    /// The largest floor seen since.
    peak_ms: f64,
    /// The most the peers delivered since (the mean of [`UPLINK_CARRIED_MS`]).
    peak_carried: f64,
    /// What the peers delivered over the second before the probe.
    before: f64,
}

/// The host uplink shared by every peer of a listen server (spec 120 § 4.2 item 8, F5): one
/// aggregate controller fed by the sum of the peers' delivery rates and the host's own uplink
/// signal (queueing delay rising on most peers at once), split with weighted max-min fairness.
#[derive(Debug, Clone, Default)]
pub struct SharedUplink {
    /// A fixed uplink in bytes per second (`server_rate_bytes`), or `None` to measure it.
    fixed: Option<f64>,
    /// The aggregate target once the uplink has shown a queue.
    target: Option<f64>,
    rising_rounds: u32,
    prev_queue_ms: f64,
    /// The median queueing delay of the latest [`QUEUE_FLOOR_ROUNDS`] updates: their smallest is
    /// the host's queue without the phase of each peer's socket reads.
    medians: std::collections::VecDeque<f64>,
    /// The same rounds' medians before the witness gate (ADR-10081 (5)): their smallest is what
    /// `prev_floor_ms` remembers, so a silenced round does not look like an empty queue.
    plain_medians: std::collections::VecDeque<f64>,
    prev_floor_ms: f64,
    last_ms: Option<u64>,
    last_cut_ms: Option<u64>,
    cuts: u64,
    /// The uplink's rate as the peers delivered it when a queue last built at the host, and
    /// when it was last confirmed (ADR-8490): the aggregate holds and grows only to
    /// [`BOTTLENECK_HEADROOM`] of it. After [`BOTTLENECK_HOLD_MS`] a probe asks the uplink again
    /// (ADR-10097); with fewer than [`UPLINK_CROWD_PEERS`] delivering peers, which cannot
    /// judge a probe, the estimate is dropped instead, as before.
    bottleneck: Option<(f64, u64)>,
    /// The sum of the peers' delivery rates at the updates of the latest [`DELIVERY_WINDOW_MS`].
    carried: std::collections::VecDeque<(u64, f64)>,
    /// The median queueing delay of the delivering peers at the latest [`QUEUE_FLOOR_ROUNDS`]
    /// updates, whatever its size: what a probe's verdict is read from.
    raw_medians: std::collections::VecDeque<f64>,
    /// Updates in a row with the delivering peers' median loss at [`LOSS_CUT`] or more.
    loss_rounds: u32,
    probe: Option<UplinkProbe>,
    /// What the aggregate sends over its target right now (0, or [`UPLINK_PROBE_GAIN`]).
    boost: f64,
    probes: u64,
    probes_confirmed: u64,
}

impl SharedUplink {
    /// A shared uplink: `fixed_bytes_per_second` 0 measures it.
    pub fn new(fixed_bytes_per_second: u32) -> Self {
        Self {
            fixed: (fixed_bytes_per_second > 0).then_some(f64::from(fixed_bytes_per_second)),
            ..Self::default()
        }
    }

    /// The aggregate target, bytes per second on the wire (`None` while unlimited). During a
    /// probe it reads [`UPLINK_PROBE_GAIN`] over the target the uplink holds (never over a fixed
    /// uplink).
    pub fn target_bytes_per_second(&self) -> Option<u32> {
        let probed = self.target.map(|t| t * (1.0 + self.boost));
        let t = match (self.fixed, probed) {
            (Some(f), Some(t)) => f.min(t),
            (Some(f), None) => f,
            (None, t) => t?,
        };
        Some(t.clamp(0.0, f64::from(u32::MAX)) as u32)
    }

    /// Aggregate cuts so far.
    pub fn cuts(&self) -> u64 {
        self.cuts
    }

    /// Probes for a faster uplink so far, and how many of them met the uplink's rate (a queue at
    /// the host): `(probes, confirmed)` (ADR-10097).
    pub fn probes(&self) -> (u64, u64) {
        (self.probes, self.probes_confirmed)
    }

    /// The uplink's rate as the peers last delivered it with a queue at the host, bytes per
    /// second on the wire (`None` before the first).
    pub fn bottleneck_bytes_per_second(&self) -> Option<f64> {
        self.bottleneck.map(|(rate, _)| rate)
    }

    /// Ends a probe: the boost stops at once.
    fn end_probe(&mut self) {
        self.probe = None;
        self.boost = 0.0;
    }

    /// Updates the aggregate from the peers and returns each peer's allowance (same order):
    /// `min(peer target, fair share)`. Called once per flush; decisions at most every
    /// [`MIN_ROUND_MS`].
    /// Every peer is its own witness of the host queue: [`SharedUplink::update_by_block`] with
    /// one address block per peer.
    pub fn update(&mut self, now_ms: u64, peers: &[UplinkPeer]) -> Vec<f64> {
        self.update_with_signal(now_ms, peers, || host_queue(peers))
    }

    /// [`SharedUplink::update`] for peers whose address blocks are known (`blocks[i]` is the
    /// block of `peers[i]`, same length): the host queue is read with one witness per block
    /// ([`host_queue_ms_by_block`], ADR-8883), so the connections of one machine count once.
    pub fn update_by_block(
        &mut self,
        now_ms: u64,
        peers: &[UplinkPeer],
        blocks: &[crate::addr_block::AddrBlock],
    ) -> Vec<f64> {
        self.update_with_signal(now_ms, peers, || host_queue_by_block(peers, blocks))
    }

    fn update_with_signal(
        &mut self,
        now_ms: u64,
        peers: &[UplinkPeer],
        signal: impl FnOnce() -> (f64, bool),
    ) -> Vec<f64> {
        if peers.is_empty() {
            return Vec::new();
        }
        let due = self
            .last_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= MIN_ROUND_MS);
        if due {
            let dt = self.last_ms.map_or(0, |t| now_ms.saturating_sub(t)) as f64;
            self.last_ms = Some(now_ms);
            let (plain, witnessed) = signal();
            let median = if witnessed { plain } else { 0.0 };
            let rising = median > QUEUE_DELAY_THRESHOLD_MS && median > self.prev_queue_ms;
            self.rising_rounds = if rising {
                self.rising_rounds + 1
            } else if median > QUEUE_DELAY_THRESHOLD_MS {
                self.rising_rounds
            } else {
                0
            };
            self.prev_queue_ms = median;
            if self.medians.len() >= QUEUE_FLOOR_ROUNDS {
                self.medians.pop_front();
            }
            self.medians.push_back(median);
            let floor = self.medians.iter().copied().fold(f64::INFINITY, f64::min);
            if self.plain_medians.len() >= QUEUE_FLOOR_ROUNDS {
                self.plain_medians.pop_front();
            }
            self.plain_medians.push_back(plain);
            let plain_floor = self.plain_medians.iter().copied().fold(f64::INFINITY, f64::min);
            // A queue builds at the host (on the floor, so phase noise is not a queue): the
            // uplink is the bottleneck, and what the peers delivered is its rate. The rise is
            // read on the floor of the ungated medians (ADR-10081 (5)): a round the witness gate
            // silenced says nothing about the host's queue, so the return from its zero to an
            // unchanged reading is no rise. Read on the gated floor it was one every time the
            // witness count crossed the gate, and each held the aggregate at
            // `BOTTLENECK_HEADROOM` of what the peers delivered again: a ratchet to the floor.
            let building = floor >= QUEUE_TREND_MIN_MS && plain_floor > self.prev_floor_ms;
            self.prev_floor_ms = plain_floor;
            let delivered: f64 = peers.iter().map(|p| p.delivery).sum();
            let demand: f64 = peers.iter().map(|p| p.target).sum();
            // What the peers delivered lately (ADR-10097): one update's sum drops with every
            // peer that leaves in it, and a hold or a cut that took it for the uplink's rate
            // (16 % under it, on the soak's seed 177) could not grow back for the whole hold.
            self.carried.push_back((now_ms, delivered));
            while self.carried.len() > 1
                && self
                    .carried
                    .front()
                    .is_some_and(|(at, _)| now_ms.saturating_sub(*at) >= DELIVERY_WINDOW_MS)
            {
                self.carried.pop_front();
            }
            let mean_over = |span: u64| {
                let (n, sum) = self
                    .carried
                    .iter()
                    .filter(|(at, _)| now_ms.saturating_sub(*at) < span)
                    .fold((0u32, 0.0), |(n, sum), (_, d)| (n + 1, sum + d));
                sum / f64::from(n.max(1))
            };
            let delivering = peers.iter().filter(|p| p.delivery > 0.0).count();
            // A crowd's readings are the host's (ADR-10097); a few peers keep the earlier form.
            let crowd = delivering >= UPLINK_CROWD_PEERS;
            let carried = if crowd {
                mean_over(UPLINK_CARRIED_MS)
            } else {
                delivered
            };
            let carried_second = mean_over(DELIVERY_WINDOW_MS);
            if self.raw_medians.len() >= QUEUE_FLOOR_ROUNDS {
                self.raw_medians.pop_front();
            }
            self.raw_medians.push_back(delivering_queue_ms(peers));
            let raw_floor = self.raw_medians.iter().copied().fold(f64::INFINITY, f64::min);
            self.loss_rounds = if host_loss(peers) >= LOSS_CUT {
                self.loss_rounds + 1
            } else {
                0
            };
            // Too few delivering peers to judge a probe: the estimate is dropped after its hold
            // and the aggregate grows freely, as before ADR-10097.
            let can_probe = crowd;
            if !can_probe
                && self
                    .bottleneck
                    .is_some_and(|(_, at)| now_ms.saturating_sub(at) >= BOTTLENECK_HOLD_MS)
            {
                self.bottleneck = None;
            }
            if let Some(probe) = self.probe.as_mut() {
                probe.peak_ms = probe.peak_ms.max(raw_floor);
                probe.peak_carried = probe.peak_carried.max(carried);
                if now_ms >= probe.until_ms {
                    self.boost = 0.0;
                }
            }
            // The host drops (ADR-10097): the median delivering peer loses a tenth of its
            // packets. An uplink that has no target yet takes one from it. Queueing delay alone
            // misses this start: a peer that joined while the host's queue already stood
            // measured its minimum round trip on top of the queue and reads none, and with half
            // the peers reading none the median is 0 while the host offers a quarter more than
            // its uplink carries (seconds 2 to 5 of listen-uplink/64 on seeds 36, 90 and 177).
            let dropping = self.target.is_none() && self.loss_rounds >= UPLINK_LOSS_ROUNDS && carried > 0.0;
            // The host starves peers (ADR-10097). A server flushes its peers in one burst per
            // tick, in a fixed order; a full drop-tail queue takes the burst's head and drops its
            // tail, so the peers served last lose every packet while the others lose none: the
            // median peer reads no loss and, having joined over the standing queue, no delay
            // (seven of 64 clients got nothing for 18 s on listen-uplink/64, seed 36, 60 s). The
            // starved peers still talk: several of them among a crowd that delivers is the host's
            // uplink, and it takes its first target.
            let starving = crowd
                && self.target.is_none()
                && carried > 0.0
                && peers.iter().filter(|p| p.starved).count() >= UPLINK_STARVED_PEERS;
            if (self.rising_rounds >= 2 || dropping || starving)
                && self
                    .last_cut_ms
                    .is_none_or(|t| now_ms.saturating_sub(t) >= 2 * MIN_ROUND_MS)
            {
                let base = self.target.map_or(carried, |t| carried.min(t));
                self.target = Some((DECREASE_FACTOR * base).max(f64::from(MIN_TARGET_BYTES_PER_SECOND)));
                if carried > 0.0 {
                    self.bottleneck = Some((carried, now_ms));
                }
                self.last_cut_ms = Some(now_ms);
                self.rising_rounds = 0;
                self.loss_rounds = 0;
                self.cuts += 1;
                self.end_probe();
            } else if building && carried > 0.0 {
                // Hold the aggregate just under what the uplink carries until the queue stops
                // growing (ADR-8490: at the full rate every second offered the link's rate). A
                // queue during a probe is the probe's answer: the uplink's rate stands.
                // With a crowd the estimate only rises here (ADR-10097): peers that leave part of
                // their shares unused deliver less than the target without the uplink being any
                // slower, and each hold at 97 % of that took the aggregate down a step (to a
                // quarter of the uplink in 14 s on seed 177). An uplink that did get slower keeps
                // its queue, and the cut above answers it.
                if self.probe.is_some() {
                    self.probes_confirmed += 1;
                }
                let rate = match self.bottleneck {
                    Some((rate, _)) if crowd => rate.max(carried),
                    _ => carried,
                };
                let ceiling = BOTTLENECK_HEADROOM * rate;
                let held = self.target.map_or(ceiling, |t| t.min(ceiling));
                self.target = Some(held.max(f64::from(MIN_TARGET_BYTES_PER_SECOND)));
                self.bottleneck = Some((rate, now_ms));
                self.end_probe();
            } else if let Some(probe) = self.probe {
                if now_ms >= probe.watch_until_ms {
                    // The verdict (ADR-10097). A probe that met the uplink's rate left a queue
                    // every peer's packets waited in: the estimate stands for another hold. One
                    // that met none while the peers delivered more than the estimate found room:
                    // the estimate rises to what they delivered and the next probe follows soon.
                    // One the peers did not take says nothing: the estimate stands.
                    let confirmed = probe.peak_ms >= probe.baseline_ms + UPLINK_PROBE_QUEUE_MS;
                    // What the peers delivered while the probe lasted: the peak is a mean over
                    // UPLINK_CARRIED_MS, of which the probe is UPLINK_PROBE_MS and the rest ran
                    // at the rate before it.
                    let span = UPLINK_CARRIED_MS.max(UPLINK_PROBE_MS) as f64;
                    let rest = span - UPLINK_PROBE_MS as f64;
                    let probed = (span * probe.peak_carried - rest * probe.before) / UPLINK_PROBE_MS as f64;
                    self.end_probe();
                    if let Some((rate, at)) = self.bottleneck.as_mut() {
                        *at = now_ms;
                        if confirmed {
                            self.probes_confirmed += 1;
                        } else if probed >= *rate * (1.0 + UPLINK_PROBE_STEP) {
                            *rate = probed.min(*rate * (1.0 + UPLINK_PROBE_GAIN));
                            *at = now_ms
                                .saturating_sub(BOTTLENECK_HOLD_MS.saturating_sub(UPLINK_PROBE_RETRY_MS));
                        }
                    }
                }
            } else if let Some(t) = self.target.as_mut()
                && floor < QUEUE_TREND_MIN_MS
                && median <= QUEUE_DELAY_THRESHOLD_MS
                && demand > *t
            {
                let ceiling = self.bottleneck.map(|(rate, _)| BOTTLENECK_HEADROOM * rate);
                let due_probe = can_probe
                    && self
                        .bottleneck
                        .is_some_and(|(_, at)| now_ms.saturating_sub(at) >= BOTTLENECK_HOLD_MS);
                if due_probe
                    && ceiling.is_some_and(|c| *t >= 0.99 * c)
                    && carried_second >= UPLINK_PROBE_FILL * *t
                {
                    // The estimate is a hold old and the aggregate sits at it: ask the uplink
                    // again with a short probe instead of growing past the estimate (ADR-10097).
                    // Growing past it found a faster uplink, and on an uplink that was no faster
                    // it sent over the uplink's rate until the queue showed: the second over the
                    // cap of KI-NT-031, every 15 s of a full listen server.
                    self.probe = Some(UplinkProbe {
                        until_ms: now_ms + UPLINK_PROBE_MS,
                        watch_until_ms: now_ms + UPLINK_PROBE_MS + UPLINK_PROBE_WATCH_MS,
                        baseline_ms: raw_floor,
                        peak_ms: 0.0,
                        peak_carried: 0.0,
                        before: carried_second,
                    });
                    self.boost = UPLINK_PROBE_GAIN;
                    self.probes += 1;
                } else if delivered >= UPLINK_VALIDATED * *t {
                    // Growth only while the host holds no queue and the peers deliver what the
                    // aggregate already allows (RFC 7661: a rate the path never carried is not
                    // validated; a shallow host buffer drops instead of queueing).
                    let grown = *t * (1.0 + INCREASE_PER_SECOND).powf(dt / 1000.0);
                    // Growth stops at BOTTLENECK_HEADROOM of the uplink's measured rate while the
                    // estimate holds (ADR-8490); it never cuts a target already over it.
                    *t = match ceiling {
                        Some(c) => grown.min(c.max(*t)),
                        None => grown,
                    };
                }
            }
        }
        let Some(total) = self.target_bytes_per_second().map(f64::from) else {
            return peers.iter().map(|p| p.target).collect();
        };
        max_min_shares(total, peers)
    }
}

/// The host uplink's own queueing delay as the shared uplink reads it from its peers, milliseconds
/// (ADR-8733, ADR-8883): the median of every peer's queueing delay, and 0 unless at least
/// [`UPLINK_SHARED_QUEUE_PEERS`] peers that delivered data in their latest round see a queue over
/// [`QUEUE_DELAY_THRESHOLD_MS`] at once (a peer that just joined, nothing delivered yet, is no
/// witness). A median over that threshold is a witness's own sample, and counts only when the
/// witnesses are a strict majority of the peers: a queue that only one or a few peers see, or only
/// half of them, sits on their own downlinks, their own controllers answer it, and an aggregate cut
/// would take the rate from every other peer too (the congested listen soak row's wedge, and spec
/// 120 § 1.1: one hostile peer harms nobody else). A median at or under the threshold, with a few
/// witnesses over it, is the reading of the peers that do not queue on links of their own
/// (ADR-10097's hold). Each peer is one witness here; [`host_queue_ms_by_block`] counts one per
/// address block (ADR-10327).
pub fn host_queue_ms(peers: &[UplinkPeer]) -> f64 {
    let (median, witnessed) = host_queue(peers);
    if witnessed { median } else { 0.0 }
}

/// The median of every peer's queueing delay, and whether the witness gate of [`host_queue_ms`]
/// passes: at least [`UPLINK_SHARED_QUEUE_PEERS`] delivering peers see a queue over
/// [`QUEUE_DELAY_THRESHOLD_MS`], and a median over that threshold also needs the witnesses to be a
/// strict majority of the peers (ADR-8883, as ADR-10327 joins it to ADR-10097).
fn host_queue(peers: &[UplinkPeer]) -> (f64, bool) {
    if peers.is_empty() {
        return (0.0, false);
    }
    let seeing = peers
        .iter()
        .filter(|p| p.delivery > 0.0 && p.queue_ms > QUEUE_DELAY_THRESHOLD_MS)
        .count();
    let mut queues: Vec<f64> = peers.iter().map(|p| p.queue_ms).collect();
    queues.sort_by(f64::total_cmp);
    let median = queues[queues.len() / 2];
    // A reading over the threshold is a witness's own sample: it counts only when the witnesses are
    // a strict majority (ADR-8883; with half of them the upper median is the slow half's downlink).
    // Under the threshold a few witnesses still open the gate for the median's reading (ADR-10097).
    let majority = 2 * seeing > peers.len();
    (
        median,
        seeing >= UPLINK_SHARED_QUEUE_PEERS && (majority || median <= QUEUE_DELAY_THRESHOLD_MS),
    )
}

/// The median queueing delay of the peers that delivered data in their latest round,
/// milliseconds, whatever its size (0 with none): the reading a shared uplink's probe is judged
/// by (ADR-10097). A probe's queue stands on top of whatever each peer already read, so the rise
/// of this median is the host's queue even when [`host_queue_ms`] reads none.
pub fn delivering_queue_ms(peers: &[UplinkPeer]) -> f64 {
    let mut queues: Vec<f64> = peers
        .iter()
        .filter(|p| p.delivery > 0.0)
        .map(|p| p.queue_ms)
        .collect();
    if queues.is_empty() {
        return 0.0;
    }
    queues.sort_by(f64::total_cmp);
    queues[queues.len() / 2]
}

/// The host uplink's own drops as the shared uplink reads them from its peers (ADR-10097): the
/// median loss fraction of the peers that delivered data in their latest round, and 0 with fewer
/// than [`UPLINK_CROWD_PEERS`] of them. A drop-tail queue that overflows at the host takes packets
/// from every peer behind it; the random loss of some clients' own links is not the median's.
pub fn host_loss(peers: &[UplinkPeer]) -> f64 {
    let mut loss: Vec<f64> = peers
        .iter()
        .filter(|p| p.delivery > 0.0)
        .map(|p| p.loss)
        .collect();
    if loss.len() < UPLINK_CROWD_PEERS {
        return 0.0;
    }
    loss.sort_by(f64::total_cmp);
    loss[loss.len() / 2]
}

/// [`host_queue_ms`] with one witness per address block (ADR-8883): the peers of one block
/// (`blocks[i]` is the block of `peers[i]`) are folded into one sample first, its delivery the
/// block's sum and its queue the largest queue among the block's delivering peers (among all of
/// them when none delivered). One machine (up to `max_peers_per_ip` connections) is then one
/// witness, so it cannot make up the [`UPLINK_SHARED_QUEUE_PEERS`] witnesses or the majority on
/// its own. A `blocks` slice of another length than `peers` reads every peer as its own block.
pub fn host_queue_ms_by_block(peers: &[UplinkPeer], blocks: &[crate::addr_block::AddrBlock]) -> f64 {
    let (median, witnessed) = host_queue_by_block(peers, blocks);
    if witnessed { median } else { 0.0 }
}

/// [`host_queue`] on the per-block samples of [`host_queue_ms_by_block`]: the median and whether
/// the witness gate passed (ADR-8883).
fn host_queue_by_block(peers: &[UplinkPeer], blocks: &[crate::addr_block::AddrBlock]) -> (f64, bool) {
    if blocks.len() != peers.len() {
        return host_queue(peers);
    }
    let mut folded: Vec<(crate::addr_block::AddrBlock, UplinkPeer)> = Vec::with_capacity(peers.len());
    for (peer, block) in peers.iter().zip(blocks) {
        match folded.iter_mut().find(|(b, _)| b == block) {
            Some((_, sample)) => {
                let queue = match (sample.delivery > 0.0, peer.delivery > 0.0) {
                    (true, false) => sample.queue_ms,
                    (false, true) => peer.queue_ms,
                    _ => sample.queue_ms.max(peer.queue_ms),
                };
                sample.delivery += peer.delivery;
                sample.queue_ms = queue;
            }
            None => folded.push((*block, *peer)),
        }
    }
    let samples: Vec<UplinkPeer> = folded.into_iter().map(|(_, sample)| sample).collect();
    host_queue(&samples)
}

/// Weighted max-min fair shares of `total` among peers demanding their targets (water filling):
/// a peer that wants less than its weighted share gets what it wants, the rest is split among the
/// others by weight.
pub fn max_min_shares(total: f64, peers: &[UplinkPeer]) -> Vec<f64> {
    let mut out = vec![0.0; peers.len()];
    let mut order: Vec<usize> = (0..peers.len()).collect();
    let weight = |i: usize| peers[i].weight.max(1e-3);
    order.sort_by(|&a, &b| (peers[a].target / weight(a)).total_cmp(&(peers[b].target / weight(b))));
    let mut left = total.max(0.0);
    let mut weights: f64 = order.iter().map(|&i| weight(i)).sum();
    for &i in &order {
        let share = left * weight(i) / weights.max(1e-9);
        let give = peers[i].target.max(0.0).min(share);
        out[i] = give;
        left -= give;
        weights -= weight(i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings with the first flight named off (it is off by default in v0.7, ADR-10231;
    /// a later release may turn it on): the tests of the controller's own dynamics from its
    /// start rate (the startup, the cuts, their recovery) run on the blind start they were
    /// written for whatever the default is; the first flight has its own tests
    /// (`the_first_flight_is_held_to_the_floor_and_starts_from_what_it_delivered`).
    fn blind(config: RateConfig) -> RateConfig {
        RateConfig {
            first_flight: false,
            ..config
        }
    }

    /// A saturating sender's round: it sends `rate` for 50 ms and loses `loss` of it (whole
    /// packets: the fraction a round cannot lose carries to the next).
    fn round(rate: f64, loss: f64, rtt: f64) -> RoundSample {
        thread_local!(static CARRY: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) });
        let sent = (rate * 0.05) as u64;
        let packets = (sent / 1_200).max(1) as u32;
        let owed = CARRY.with(|c| c.get()) + f64::from(packets) * loss;
        let lost = (owed.floor() as u32).min(packets);
        CARRY.with(|c| c.set(owed - f64::from(lost)));
        RoundSample {
            interval_ms: 50,
            acked_bytes: sent * u64::from(packets - lost) / u64::from(packets),
            sent_bytes: sent,
            acked_packets: packets - lost,
            lost_packets: lost,
            round_min_rtt_ms: rtt,
            srtt_ms: rtt,
            min_rtt_ms: rtt,
            delay_samples: 0,
            round_min_one_way_ms: 0.0,
            min_one_way_ms: 0.0,
            gradient: None,
            held_bytes: 1,
            capped: false,
            essential_bytes: 0,
        }
    }

    fn settle(c: &mut RateController, now: &mut u64, rate: Option<f64>, loss: f64, ms: u64) {
        let end = *now + ms;
        while *now < end {
            *now += 50;
            let r = rate.unwrap_or(f64::from(c.target_bytes_per_second()));
            c.on_round(*now, &round(r, loss, 40.0));
        }
    }

    /// Spec 120 § 4.2 item 5 (DO27 test A4): 10 % congestive loss for 10 s takes a saturating
    /// sender to the floor within 3 s; once it clears, the rate is back within 5 s. Congestive
    /// loss comes with its evidence (item 3): here the full drop-tail buffer behind it, a queue
    /// standing 30 ms over the minimum round trip. The same loss without a queue that does not
    /// follow the rate down is random loss
    /// (`random_loss_that_does_not_follow_the_rate_keeps_a_saturating_sender`).
    #[test]
    fn ten_percent_loss_reaches_the_floor_in_three_seconds_and_recovers_in_five() {
        let mut c = RateController::new(blind(RateConfig::client()));
        let mut now = 0;
        // A saturating sender at the 256 KiB/s start rate.
        let before = c.target_bytes_per_second();
        assert_eq!(before, 262_144);
        let floor = c.floor_bytes_per_second();
        let start = now;
        let mut at_floor = None;
        // The path's minimum round trip is 40 ms; the full buffer adds 30 ms to every packet.
        while now < start + 10_000 {
            now += 50;
            let mut s = round(f64::from(c.target_bytes_per_second()), 0.10, 40.0);
            s.round_min_rtt_ms = 70.0;
            s.srtt_ms = 70.0;
            c.on_round(now, &s);
            if at_floor.is_none() && c.target_bytes_per_second() <= floor {
                at_floor = Some(now - start);
            }
        }
        let at_floor = at_floor.expect("the floor is reached");
        println!("floor {floor} reached after {at_floor} ms (from {before})");
        assert!(at_floor <= 3_000, "floor after {at_floor} ms");
        let clear = now;
        let mut back = None;
        while now < clear + 5_000 {
            settle(&mut c, &mut now, None, 0.0, 50);
            if back.is_none() && c.target_bytes_per_second() >= before {
                back = Some(now - clear);
            }
        }
        let back = back.expect("the rate comes back within 5 s");
        println!("back to {before} after {back} ms");
        assert!(back <= 5_000);
    }

    /// Item 3 (F7, ADR-8301): 10 % loss with no queue that stays when the rate is cut is random
    /// (wireless) loss, not a policer: the probe cut is given back and a saturating sender keeps
    /// its rate (the transport's `random_loss_does_not_shrink_the_window`).
    #[test]
    fn random_loss_that_does_not_follow_the_rate_keeps_a_saturating_sender() {
        let mut c = RateController::new(RateConfig {
            max_bytes_per_second: 1_000_000,
            ..RateConfig::client()
        });
        let mut now = 0;
        let (mut sum, mut n) = (0.0, 0.0);
        while now < 10_000 {
            settle(&mut c, &mut now, None, 0.10, 50);
            sum += f64::from(c.target_bytes_per_second());
            n += 1.0;
        }
        let mean = sum / n;
        println!("mean target {mean:.0}, cuts {}", c.cuts());
        assert!(mean >= 800_000.0, "mean target {mean:.0} of 1,000,000");
        assert!(c.cuts() <= 3, "{} cuts", c.cuts());
    }

    /// ADR-8490: a slow sender's few round trips on a jittery path are no queue evidence. A path
    /// with a 100 ms minimum round trip, 0-75 ms of jitter each way and 25 % random loss, no rate
    /// limit: the smallest of one or two round trips sits 30-110 ms over the minimum, and read as a
    /// standing queue it made every lossy round a congestion cut, down to the minimum rate. Over
    /// 60 s the saturating sender must keep at least half its 100 kB/s cap over the last 30 s.
    #[test]
    fn a_few_jittery_round_trips_are_no_queue_evidence() {
        let mut c = RateController::new(RateConfig {
            max_bytes_per_second: 100_000,
            ..RateConfig::client()
        });
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut unit = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % 1_000) as f64 / 1_000.0
        };
        let (mut now, mut carry) = (0u64, 0.0f64);
        let (mut sum, mut n, mut lowest) = (0.0, 0.0, u32::MAX);
        while now < 60_000 {
            now += 200;
            let rate = f64::from(c.target_bytes_per_second());
            let sent = (rate * 0.2) as u64;
            let packets = (sent / 1_200).max(1) as u32;
            carry += f64::from(packets) * 0.25;
            let lost = (carry.floor() as u32).min(packets);
            carry -= f64::from(lost);
            let acked = packets - lost;
            // Each acknowledged packet's round trip: the minimum plus both directions' jitter.
            let round_min = (0..acked)
                .map(|_| 100.0 + 75.0 * (unit() + unit()))
                .fold(f64::INFINITY, f64::min);
            c.on_round(
                now,
                &RoundSample {
                    interval_ms: 200,
                    acked_bytes: sent * u64::from(acked) / u64::from(packets),
                    sent_bytes: sent,
                    acked_packets: acked,
                    lost_packets: lost,
                    round_min_rtt_ms: if acked > 0 { round_min } else { 0.0 },
                    srtt_ms: 175.0,
                    min_rtt_ms: 100.0,
                    delay_samples: 0,
                    round_min_one_way_ms: 0.0,
                    min_one_way_ms: 0.0,
                    gradient: None,
                    held_bytes: 1,
                    capped: false,
                    essential_bytes: 0,
                },
            );
            if now > 30_000 {
                sum += f64::from(c.target_bytes_per_second());
                n += 1.0;
                lowest = lowest.min(c.target_bytes_per_second());
            }
        }
        let mean = sum / n;
        println!("mean target {mean:.0}, lowest {lowest}, cuts {}", c.cuts());
        assert!(mean >= 50_000.0, "mean target {mean:.0} of 100,000");
    }

    /// A saturating sender through a drop-tail bottleneck in 100 ms rounds (a fluid model):
    /// `cap(now)` bytes per second, a buffer of 150 ms at that rate and a 60 ms minimum round
    /// trip; the round's smallest round trip carries the queue its first byte found. Returns the
    /// bytes offered in each whole second.
    fn drop_tail(c: &mut RateController, cap: impl Fn(u64) -> f64, ms: u64) -> Vec<f64> {
        let (mut queue, mut now) = (0.0f64, 0u64);
        let mut seconds = vec![0.0; ms.div_ceil(1_000) as usize];
        while now < ms {
            let rate = cap(now);
            let offered = f64::from(c.target_bytes_per_second()) * 0.1;
            seconds[(now / 1_000) as usize] += offered;
            let first_rtt = 60.0 + 1_000.0 * queue / rate;
            let served = (queue + offered).min(rate * 0.1);
            queue += offered - served;
            let dropped = (queue - 0.15 * rate).max(0.0);
            queue -= dropped;
            let packets = (offered / 1_200.0).ceil().max(1.0) as u32;
            let lost = ((dropped / 1_200.0).round() as u32).min(packets);
            now += 100;
            c.on_round(
                now,
                &RoundSample {
                    interval_ms: 100,
                    acked_bytes: served as u64,
                    sent_bytes: offered as u64,
                    acked_packets: packets - lost,
                    lost_packets: lost,
                    round_min_rtt_ms: first_rtt,
                    srtt_ms: 60.0 + 1_000.0 * queue / rate,
                    min_rtt_ms: 60.0,
                    delay_samples: 0,
                    round_min_one_way_ms: 0.0,
                    min_one_way_ms: 0.0,
                    gradient: None,
                    held_bytes: 1,
                    capped: false,
                    essential_bytes: 0,
                },
            );
        }
        seconds
    }

    /// ADR-8490: headroom under the bottleneck. A saturating sender through a 100 kB/s
    /// drop-tail link: once a cut has measured the link, growth stops at 97 % of it, so while the
    /// estimate holds (from 5 s to 15 s) no whole second offers more than the link carries, and
    /// the sender keeps at least 85 % of it. After the hold, growth tests the link again.
    #[test]
    fn growth_stops_under_the_bottleneck_estimate() {
        let mut c = RateController::new(RateConfig::default());
        let seconds = drop_tail(&mut c, |_| 100_000.0, 15_000);
        println!("{seconds:.0?}");
        let late = &seconds[5..];
        let worst = late.iter().copied().fold(0.0, f64::max);
        let mean = late.iter().sum::<f64>() / late.len() as f64;
        assert!(worst <= 100_000.0, "a second offered {worst:.0} of 100,000");
        assert!(mean >= 85_000.0, "mean {mean:.0} of 100,000");
    }

    /// ADR-8490: the bottleneck estimate lapses, so a link that got faster is found: 100 kB/s
    /// for 20 s, then 200 kB/s. Within the hold and 15 s of growth after it the sender offers
    /// clearly more than the old link's rate.
    #[test]
    fn a_faster_link_is_found_after_the_hold() {
        let mut c = RateController::new(RateConfig::default());
        let ms = 20_000 + BOTTLENECK_HOLD_MS + 15_000;
        let seconds = drop_tail(&mut c, |t| if t < 20_000 { 100_000.0 } else { 200_000.0 }, ms);
        println!("{seconds:.0?}");
        let last = seconds.last().copied().unwrap_or(0.0);
        assert!(last >= 130_000.0, "the last second offered {last:.0}");
    }

    /// ADR-8400: a round without a round-trip sample of its own (heavy loss: its acknowledged
    /// packets carried resent data, which is not sampled) is no queue evidence. Two rounds read a
    /// 60 ms queue, then 20 % random loss arrives with no samples for 3 s: the stale reading
    /// must not turn every round into a congestion cut (it took a saturating sender to the
    /// minimum rate within a second), so the loss is probed, found random and the rate kept.
    #[test]
    fn a_round_without_a_sample_is_no_queue_evidence() {
        let mut c = RateController::new(RateConfig {
            max_bytes_per_second: 100_000,
            ..RateConfig::client()
        });
        let mut now = 0;
        settle(&mut c, &mut now, None, 0.0, 1_000);
        for _ in 0..2 {
            now += 50;
            let mut s = round(f64::from(c.target_bytes_per_second()), 0.0, 40.0);
            s.round_min_rtt_ms = 100.0;
            c.on_round(now, &s);
        }
        let before = c.target_bytes_per_second();
        let (mut lowest, end) = (before, now + 3_000);
        while now < end {
            now += 50;
            let mut s = round(f64::from(c.target_bytes_per_second()), 0.20, 40.0);
            s.round_min_rtt_ms = 0.0;
            c.on_round(now, &s);
            lowest = lowest.min(c.target_bytes_per_second());
        }
        let after = c.target_bytes_per_second();
        println!(
            "before {before}, lowest {lowest}, after {after}, cuts {}",
            c.cuts()
        );
        assert!(
            u64::from(after) * 2 >= u64::from(before),
            "kept at least half the rate: {after} of {before}"
        );
        assert!(c.cuts() <= 3, "{} cuts", c.cuts());
    }

    /// Item 3: a policer drops what is over its rate, so the loss follows the rate down: the
    /// cut to the delivery rate stands (it is not taken for random loss and given back).
    #[test]
    fn a_policers_loss_cut_stands() {
        let mut c = RateController::new(RateConfig::default());
        let policer = 100_000.0;
        let mut now = 0;
        for _ in 0..14 {
            now += 50;
            let sent = f64::from(c.target_bytes_per_second());
            let mut s = round(sent, 0.0, 40.0);
            let delivered = sent.min(policer);
            let lost = (f64::from(s.acked_packets) * (1.0 - delivered / sent)).round() as u32;
            s.acked_packets -= lost;
            s.lost_packets = lost;
            s.acked_bytes = (delivered * 0.05) as u64;
            c.on_round(now, &s);
        }
        assert!(c.cuts() >= 1);
        let t = c.target_bytes_per_second();
        assert!(f64::from(t) <= 1.1 * policer, "the cut was given back: {t}");
    }

    /// A game's 100 ms round at 30 Hz: three ticks of two 873-byte packets, `lost` of them lost.
    fn game_round(interval_ms: u64, sent: u64, acked: u32, lost: u32) -> RoundSample {
        RoundSample {
            interval_ms,
            acked_bytes: 873 * u64::from(acked),
            sent_bytes: sent,
            acked_packets: acked,
            lost_packets: lost,
            round_min_rtt_ms: 66.0,
            srtt_ms: 66.5,
            min_rtt_ms: 66.0,
            // No arrival stamps (ADR-10050): the round trips decide, as on the base these rounds
            // were replayed from (ADR-10157 was written before wire 6's one-way delay).
            delay_samples: 0,
            round_min_one_way_ms: 0.0,
            min_one_way_ms: 0.0,
            gradient: None,
            held_bytes: 0,
            capped: false,
            essential_bytes: 0,
        }
    }

    /// ADR-10157: a round that holds one tick of sends more than its length is not a saturating
    /// sender. The rounds of the server's controller toward client 1 of the soak row
    /// lossy-fat-pipe/8 at seed 36, replayed: a game at 52 kB/s under a target of about 80 kB/s
    /// (the app-limited bound), 5 % random loss with no queue. The loss memory reads over 10 %
    /// in a round that ended 68 ms after it began, just after a tick, with three ticks' sends in
    /// it (77 kB/s for that round): that was a loss cut to the round's delivery, the first of
    /// four that took the snapshot budget to 14 of 140 entities.
    #[test]
    fn a_round_with_an_extra_tick_of_sends_is_not_a_saturating_sender() {
        let mut c = RateController::new(RateConfig::default());
        let mut now = 0;
        for _ in 0..12 {
            now += 100;
            c.on_round(now, &game_round(100, 5_238, 6, 0));
        }
        // (length, bytes sent, acknowledged, lost) as traced from 4,334 ms to 5,500 ms.
        for (interval, sent, acked, lost) in [
            (100, 5_238, 5, 2),
            (100, 5_300, 6, 0),
            (100, 5_238, 6, 0),
            (100, 5_238, 6, 0),
            (100, 5_300, 6, 0),
            (100, 5_238, 6, 0),
            (100, 5_300, 6, 0),
            (166, 8_734, 9, 1),
            (100, 5_300, 5, 0),
            (100, 5_238, 6, 1),
            (100, 5_238, 5, 1),
            (100, 5_302, 4, 1),
        ] {
            now += interval;
            c.on_round(now, &game_round(interval, sent, acked, lost));
        }
        let before = c.target_bytes_per_second();
        assert_eq!(c.cuts(), 0);
        // The round of 5,568 ms: 68 ms, three ticks' sends, one packet of five lost.
        now += 68;
        let aliased = game_round(68, 5_253, 4, 1);
        let round_rate = aliased.sent_bytes as f64 * 1_000.0 / 68.0;
        assert!(
            round_rate >= 0.8 * f64::from(before),
            "the round alone reads as saturating: {round_rate:.0} of {before}"
        );
        c.on_round(now, &aliased);
        assert!(
            c.loss_fraction() >= LOSS_CUT,
            "the loss memory reads {:.3}",
            c.loss_fraction()
        );
        let after = c.target_bytes_per_second();
        println!(
            "target {before} -> {after}, loss {:.3}, cuts {}",
            c.loss_fraction(),
            c.cuts()
        );
        assert_eq!(
            c.cuts(),
            0,
            "a sender at two thirds of its target was cut: {before} -> {after}"
        );
        assert!(after >= before, "{before} -> {after}");
    }

    /// What [`tick_game`] measured from 5 s on.
    struct TickGame {
        cuts: u64,
        lowest: u32,
        mean: f64,
        last: u32,
    }

    /// A game that sends two-tick-old state at 30 Hz in 873-byte packets through a path with
    /// `loss` random loss, no queue and a 66 ms round trip, fed to the controller as the
    /// transport feeds it: a round ends on the first acknowledgment at least 67 ms (the smoothed
    /// round trip, rounded up) after the one before, and an acknowledgment arrives 66 to 68 ms
    /// after its packet left, so some rounds end just before a tick's sends and some just after
    /// (one tick more or less in the round). The game sends `demand` bytes per second or its
    /// target, whichever is lower, in whole packets. Seeded: every decision replays.
    fn tick_game(c: &mut RateController, seed: u64, loss: f64, demand: f64, ms: u64) -> TickGame {
        const PACKET: u64 = 873;
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut unit = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1u64 << 53) as f64
        };
        // When each packet in flight is resolved: (acknowledged, declared lost) per millisecond.
        let mut due = std::collections::BTreeMap::<u64, (u32, u32)>::new();
        let mut round = (None::<u64>, 0u32, 0u32, 0u64);
        let mut owed = 0.0;
        let mut out = TickGame {
            cuts: 0,
            lowest: u32::MAX,
            mean: 0.0,
            last: 0,
        };
        let mut samples = 0.0;
        for tick in 0..ms * 3 / 100 {
            let now = tick * 100 / 3;
            while let Some(entry) = due.first_entry().filter(|e| *e.key() <= now) {
                let (at, (acked, lost)) = entry.remove_entry();
                round.1 += acked;
                round.2 += lost;
                let start = *round.0.get_or_insert(at);
                if acked > 0 && at - start >= 67 {
                    c.on_round(at, &game_round(at - start, round.3, round.1, round.2));
                    round = (Some(at), 0, 0, 0);
                }
            }
            owed += demand.min(f64::from(c.target_bytes_per_second())) / 30.0;
            while owed >= PACKET as f64 {
                owed -= PACKET as f64;
                round.3 += PACKET;
                let jitter = (unit() * 3.0) as u64;
                if unit() < loss {
                    // Declared lost when the next tick's packets are acknowledged.
                    due.entry(now + 100 + jitter).or_default().1 += 1;
                } else {
                    due.entry(now + 66 + jitter).or_default().0 += 1;
                }
            }
            if now >= 5_000 {
                let t = c.target_bytes_per_second();
                out.lowest = out.lowest.min(t);
                out.mean += f64::from(t);
                samples += 1.0;
            }
        }
        out.cuts = c.cuts();
        out.mean /= f64::max(samples, 1.0);
        out.last = c.target_bytes_per_second();
        out
    }

    /// ADR-10157: a game under its target keeps its rate through random loss whatever the phase
    /// of its ticks against the feedback rounds. 52 kB/s of snapshots at 30 Hz, 5 % random loss,
    /// 60 s, 32 seeds: the controller never cuts (before, a round with one tick of sends more
    /// than its length read as a saturating sender, and the loss memory's 10 % readings cut the
    /// rate in every one of these seeds).
    #[test]
    fn a_tick_quantized_game_under_its_target_is_never_cut_for_random_loss() {
        let mut cut = Vec::new();
        for seed in 1..=32 {
            let mut c = RateController::new(RateConfig::default());
            let g = tick_game(&mut c, seed, 0.05, 52_380.0, 60_000);
            if g.cuts > 0 || g.lowest < 52_380 {
                cut.push((seed, g.cuts, g.lowest));
            }
        }
        assert!(cut.is_empty(), "(seed, cuts, lowest target): {cut:?}");
    }

    /// ADR-10200 (7), ADR-10231: the same game after a first flight (opt-in). The rounds whose
    /// packets the flight held to the floor's rate are not what the game uses: the app-limited
    /// bound leaves them out, so the target stays over the game's 52 kB/s and random loss never
    /// cuts it. Before ADR-10200's fix the bound read those rounds (target 64,740 B/s at seed 6)
    /// and 6 of these 32 seeds were cut.
    #[test]
    fn a_tick_quantized_game_after_a_first_flight_is_never_cut_for_random_loss() {
        let mut cut = Vec::new();
        for seed in 1..=32 {
            let mut c = RateController::new(RateConfig {
                first_flight: true,
                ..RateConfig::default()
            });
            assert!(c.first_flight());
            let g = tick_game(&mut c, seed, 0.05, 52_380.0, 60_000);
            if g.cuts > 0 || g.lowest < 52_380 {
                cut.push((seed, g.cuts, g.lowest));
            }
        }
        assert!(cut.is_empty(), "(seed, cuts, lowest target): {cut:?}");
    }

    /// ADR-10157: a thin sender that fills its target keeps it through random loss. The same game
    /// with a project rate of 52.4 kB/s, which it fills (a game that sizes its snapshots to the
    /// allowance): 5 % random loss with no queue, 120 s, 32 seeds. Before, every 10 % reading of
    /// the loss memory cut the target to one round's delivery and two probes in three ruled the
    /// loss had followed the rate, so every seed ended at the minimum rate.
    #[test]
    fn a_thin_saturating_sender_keeps_its_rate_through_random_loss() {
        let cap = 52_400;
        let mut worst = Vec::new();
        let (mut mean_all, mut lowest_all) = (0.0, u32::MAX);
        for seed in 1..=32 {
            let mut c = RateController::new(RateConfig {
                max_bytes_per_second: cap,
                ..RateConfig::default()
            });
            let g = tick_game(&mut c, seed, 0.05, f64::INFINITY, 120_000);
            println!(
                "seed {seed}: mean {:.0}, lowest {}, last {}, cuts {}",
                g.mean, g.lowest, g.last, g.cuts
            );
            mean_all += g.mean / 32.0;
            lowest_all = lowest_all.min(g.lowest);
            if g.mean < 0.9 * f64::from(cap) || g.lowest < cap / 2 {
                worst.push((seed, g.mean as u32, g.lowest, g.cuts));
            }
        }
        println!("all seeds: mean {mean_all:.0}, lowest {lowest_all} of {cap}");
        assert!(worst.is_empty(), "(seed, mean, lowest, cuts) of {cap}: {worst:?}");
    }

    /// Item 3 (F7): 5 % random loss with no queue on a path that delivers what is sent never cuts
    /// the rate, and 30 % loss without queueing on an app-limited sender does not either.
    #[test]
    fn random_loss_without_queueing_is_not_a_cut() {
        let mut c = RateController::new(blind(RateConfig::default()));
        let mut now = 0;
        let start = c.target_bytes_per_second();
        // An app-limited game at 127 KB/s through a lossy fat pipe.
        for _ in 0..400 {
            now += 50;
            let mut s = round(127_000.0, 0.05, 40.0);
            s.held_bytes = 0;
            c.on_round(now, &s);
        }
        assert_eq!(c.cuts(), 0);
        // The app-limited bound keeps 1.5x what the path delivers: the game's 127 KB/s still fit.
        let t = c.target_bytes_per_second();
        assert!(t >= 175_000 && t <= start, "{t}");
        for _ in 0..100 {
            now += 50;
            let mut s = round(127_000.0, 0.3, 40.0);
            s.held_bytes = 0;
            c.on_round(now, &s);
        }
        assert_eq!(c.cuts(), 0, "loss on an underused path is not congestion");
    }

    /// ADR-8592: a steady game under its start rate ends its startup after a whole second of
    /// data, not before. Within that second the startup goes on under the app-limited bound
    /// (the input stream below needs it); after it the game's rate is measured, so a transfer that
    /// starts later grows from there instead of doubling into a deep buffer (with the startup kept
    /// for good, 28 of 100 bufferbloat+bulk soak seeds put a whole second over the 1 Mbit/s link,
    /// worst 160.8 %).
    #[test]
    fn a_steady_game_ends_its_startup_after_a_second_of_data() {
        let mut c = RateController::new(RateConfig::default());
        let mut now = 0;
        let mut first_second = Vec::new();
        for _ in 0..60 {
            now += 50;
            let mut s = round(56_000.0, 0.0, 40.0);
            s.held_bytes = 0;
            c.on_round(now, &s);
            if now < 1_000 {
                first_second.push(c.state());
            }
        }
        assert!(
            first_second.iter().all(|s| *s == RateState::Startup),
            "{first_second:?}"
        );
        assert_ne!(c.state(), RateState::Startup);
        // The bound (1.5x the game's 56 kB/s plus 10 kbit/s, 85,250), then growth at
        // INCREASE_PER_SECOND for two seconds: far under the 256 KiB/s start rate.
        let t = c.target_bytes_per_second();
        assert!((85_000..=110_000).contains(&t), "{t}");
    }

    /// ADR-8592 (the x86-64 `input_mtu::large_samples_survive_ten_percent_loss` red): a client
    /// whose input bundles (essential, three copies at 10 % loss) offer 230 kB/s, and whose first
    /// data round was half empty, on a path with 7 % random loss and no queue. The app-limited
    /// bound lowers the unconfirmed 256 KiB/s start to 1.5x that first round; the startup must
    /// then still read the pipe from the rounds the inputs fill, so that after one second the
    /// pacer (1.25x the target, plus the essential floor) lets every bundle through. Before
    /// ADR-8592 the bound ended the startup at 151 kB/s, the 2-10 % loss band held it there, and
    /// every round after the first held inputs back (they expire, unsent, a tick later).
    #[test]
    fn an_input_stream_over_its_first_round_is_not_held_under_its_demand() {
        let mut c = RateController::new(RateConfig::client());
        let floor = f64::from(c.floor_bytes_per_second());
        let demand = 230_000.0;
        let (mut now, mut carry, mut short) = (0, 0.0, 0);
        for i in 0..40u32 {
            now += 50;
            let offered = if i == 0 { 100_000.0 } else { demand };
            let pass = PACING_GAIN * f64::from(c.target_bytes_per_second()) + floor;
            let held = offered > pass;
            if i >= 20 && held {
                short += 1;
            }
            let sent = (offered.min(pass) * 0.05) as u64;
            let packets = (sent / 1_150).max(1) as u32;
            carry += f64::from(packets) * if i == 0 { 0.0 } else { 0.07 };
            let lost = (carry.floor() as u32).min(packets);
            carry -= f64::from(lost);
            c.on_round(
                now,
                &RoundSample {
                    interval_ms: 50,
                    acked_bytes: sent * u64::from(packets - lost) / u64::from(packets),
                    sent_bytes: sent,
                    acked_packets: packets - lost,
                    lost_packets: lost,
                    round_min_rtt_ms: 1.0,
                    srtt_ms: 1.0,
                    min_rtt_ms: 1.0,
                    delay_samples: 0,
                    round_min_one_way_ms: 0.0,
                    min_one_way_ms: 0.0,
                    gradient: None,
                    held_bytes: u64::from(held),
                    capped: false,
                    essential_bytes: sent,
                },
            );
        }
        let t = c.target_bytes_per_second();
        assert_eq!(
            short, 0,
            "rounds of the second second that held inputs back (target {t})"
        );
        assert_eq!(c.cuts(), 0, "random loss without a queue is no cut");
        // The target follows the demand; it is not a runaway promise.
        assert!(f64::from(t) <= 2.0 * demand, "target {t}");
    }

    /// Item 2: a queue that grows for two rounds cuts the target to 0.85x the delivery rate.
    #[test]
    fn a_growing_queue_cuts_to_the_delivery_rate() {
        let mut c = RateController::new(RateConfig::default());
        let mut now = 0;
        let mut rtt = 40.0;
        // 64 KB/s bottleneck: the sender sends 256 KiB/s, the path delivers 64 KB/s.
        for _ in 0..3 {
            now += 50;
            rtt += 30.0;
            let mut s = round(262_144.0, 0.0, rtt);
            s.acked_bytes = 3_200;
            s.min_rtt_ms = 40.0;
            c.on_round(now, &s);
        }
        assert_eq!(c.cuts(), 1, "one cut per round trip");
        assert!(matches!(c.state(), RateState::Decrease | RateState::Hold));
        let t = c.target_bytes_per_second();
        assert!((50_000..=56_000).contains(&t), "{t}");
        assert!(c.queue_delay_ms() > QUEUE_DELAY_THRESHOLD_MS);
    }

    /// The target after a queue showed for two rounds on a path carrying 16 kB/s (100 ms rounds
    /// of three packets), the second of which acknowledged one 525-byte packet.
    fn target_after_a_thin_overuse_round(shared_uplink: bool) -> u32 {
        let mut c = RateController::new(RateConfig {
            start_bytes_per_second: 16_000,
            ..RateConfig::default()
        });
        c.set_shared_uplink(shared_uplink);
        let steady = |rtt: f64, acked: u64, packets: u32| RoundSample {
            interval_ms: 100,
            acked_bytes: acked,
            sent_bytes: 1_600,
            acked_packets: packets,
            lost_packets: 0,
            round_min_rtt_ms: rtt,
            srtt_ms: 80.0,
            min_rtt_ms: 66.0,
            delay_samples: 0,
            round_min_one_way_ms: 0.0,
            min_one_way_ms: 0.0,
            gradient: None,
            held_bytes: 4,
            capped: false,
            essential_bytes: 0,
        };
        let mut now = 0;
        for _ in 0..20 {
            now += 100;
            c.on_round(now, &steady(66.0, 1_600, 3));
        }
        assert_eq!(c.cuts(), 0);
        let before = c.target_bytes_per_second();
        assert!(
            (15_000..=20_000).contains(&before),
            "settled at the path's rate: {before}"
        );
        now += 100;
        c.on_round(now, &steady(100.0, 1_600, 3));
        now += 100;
        c.on_round(now, &steady(122.0, 525, 1));
        assert_eq!(c.cuts(), 1, "the growing queue cuts");
        c.target_bytes_per_second()
    }

    /// ADR-8859: one thin round does not set a cut. A queue shows for two rounds and the second
    /// acknowledged one packet of 525 bytes (the other acknowledgments fell into the next round).
    /// The cut is 0.85x the delivery rate of the latest second, not 0.85x that round's 5 kB/s.
    /// Before, the target fell from 17.0 to 4.5 kB/s on a link carrying 16 (soak congested+bulk
    /// seed 640, the server's side) and needed 16 s at 8 %/s to grow back; every reply waited at
    /// the pacer.
    #[test]
    fn one_thin_round_does_not_set_an_overuse_cut() {
        let t = target_after_a_thin_overuse_round(false);
        assert!(
            (12_000..=14_000).contains(&t),
            "0.85x the latest second's ~15 kB/s, not 0.85x one round's 5.25 kB/s: {t}"
        );
    }

    /// ADR-8859: under a shared uplink the round alone still sets the cut. The aggregate hands
    /// out the whole uplink, and the peers' own cuts are part of what keeps the sum under it.
    #[test]
    fn under_a_shared_uplink_the_round_still_sets_the_cut() {
        let t = target_after_a_thin_overuse_round(true);
        assert!((4_096..=5_000).contains(&t), "0.85x the round's 5.25 kB/s: {t}");
    }

    /// Jitter that delays some packets of a round is not a queue: the round's smallest sample
    /// decides.
    #[test]
    fn jitter_is_not_queueing() {
        let mut c = RateController::new(RateConfig::default());
        for i in 0..100u64 {
            let mut s = round(100_000.0, 0.0, 40.0);
            s.held_bytes = 0;
            s.round_min_rtt_ms = 40.0 + (i % 3) as f64;
            c.on_round(i * 50, &s);
        }
        assert_eq!(c.cuts(), 0);
    }

    /// Item 6: the start is `min(project rate, 256 KiB/s)` and the startup doubles only while the
    /// delivery rate keeps up, never past the project rate.
    #[test]
    fn startup_is_bounded() {
        let c = RateController::new(RateConfig {
            max_bytes_per_second: 49_152,
            ..blind(RateConfig::default())
        });
        assert_eq!(c.target_bytes_per_second(), 49_152);
        let mut c = RateController::new(blind(RateConfig::default()));
        assert_eq!(c.target_bytes_per_second(), 262_144);
        let mut now = 0;
        // The path delivers 300 KB/s whatever is sent: two flat rounds end the startup.
        for _ in 0..10 {
            now += 50;
            let mut s = round(f64::from(c.target_bytes_per_second()), 0.0, 40.0);
            s.acked_bytes = 15_000;
            c.on_round(now, &s);
        }
        assert_ne!(c.state(), RateState::Startup);
        assert!(
            c.target_bytes_per_second() <= 4 * 262_144,
            "{}",
            c.target_bytes_per_second()
        );
    }

    /// Item 4: a path that carries less than the floor at over 50 % loss for the breaker time trips
    /// the breaker; the first good round clears it. The target never goes under the minimum.
    #[test]
    fn the_circuit_breaker_trips_after_its_time() {
        let mut c = RateController::new(RateConfig::client());
        let mut now = 0;
        for _ in 0..(10_000 / 50) {
            now += 50;
            c.on_round(now, &round(8_000.0, 0.8, 40.0));
        }
        assert!(!c.breaker_tripped(), "not before 10 s");
        for _ in 0..4 {
            now += 50;
            c.on_round(now, &round(8_000.0, 0.8, 40.0));
        }
        assert!(c.breaker_tripped());
        assert!(c.target_bytes_per_second() >= MIN_TARGET_BYTES_PER_SECOND);
        now += 50;
        c.on_round(now, &round(40_000.0, 0.0, 40.0));
        assert!(!c.breaker_tripped());
    }

    /// Item 7: the first burst is the initial window, later ones at most one tick of the pacing
    /// rate, and the pacer says when the next packet may leave.
    #[test]
    fn the_pacer_spreads_packets_at_one_and_a_quarter_times_the_target() {
        let mut p = Pacer::new(16 * 1_200);
        let rate = 64_000.0;
        let depth = p.depth(rate, 16, 1, 1_200);
        // One tick at 64 KB/s and 16 ms is 1,280 B: the floor of PACING_BURST_PACKETS packets
        // (four since ADR-8692) sets the depth.
        assert_eq!(
            depth,
            (PACING_BURST_PACKETS * 1_200) as f64,
            "the burst floor at 64 KB/s and 16 ms ticks"
        );
        p.refill(0, rate, depth);
        let mut first = 0;
        while p.ready() {
            p.spend(1_200);
            first += 1;
        }
        assert_eq!(first, 16);
        // One second later the bucket holds one tick's depth, not a second of tokens.
        p.refill(1_000, rate, depth);
        let mut burst = 0;
        while p.ready() {
            p.spend(1_200);
            burst += 1;
        }
        assert!(burst <= PACING_BURST_PACKETS + 1, "{burst}");
        let next = p.next_send_at_ms(1_000, rate);
        assert!(next > 1_000 && next <= 1_030, "{next}");
        // Over ten seconds the bytes sent are 1.25x the rate.
        let mut sent = 0usize;
        for now in 1_001..11_001 {
            p.refill(now, rate, depth);
            while p.ready() {
                p.spend(1_200);
                sent += 1_200;
            }
        }
        let per_s = sent as f64 / 10.0;
        assert!((per_s - 80_000.0).abs() < 1_500.0, "{per_s}");
    }

    /// Item 8: weighted max-min shares: small demands are met, the rest splits by weight.
    #[test]
    fn max_min_shares_meet_small_demands_first() {
        let peer = |target: f64, weight: f64| UplinkPeer {
            target,
            delivery: target,
            queue_ms: 0.0,
            weight,
            loss: 0.0,
            starved: false,
        };
        let shares = max_min_shares(
            300.0,
            &[
                peer(50.0, 1.0),
                peer(500.0, 1.0),
                peer(500.0, 1.0),
                peer(500.0, 0.25),
            ],
        );
        assert_eq!(shares[0], 50.0);
        assert!(
            (shares[1] - 111.1).abs() < 0.2 && (shares[2] - 111.1).abs() < 0.2,
            "{shares:?}"
        );
        assert!((shares[3] - 27.8).abs() < 0.2, "{shares:?}");
        assert!((shares.iter().sum::<f64>() - 300.0).abs() < 1e-6);
    }

    /// Item 8: a queue rising on most peers at once cuts the aggregate; the shares then sum to it.
    #[test]
    fn a_shared_queue_cuts_the_aggregate() {
        let mut u = SharedUplink::new(0);
        let mut now = 0;
        let mut q = 0.0;
        let mut shares = Vec::new();
        for _ in 0..6 {
            now += 50;
            q += 20.0;
            let peers: Vec<UplinkPeer> = (0..64)
                .map(|_| UplinkPeer {
                    target: 262_144.0,
                    delivery: 40_000.0,
                    queue_ms: q,
                    weight: 1.0,
                    loss: 0.0,
                    starved: false,
                })
                .collect();
            shares = u.update(now, &peers);
        }
        let total = u.target_bytes_per_second().expect("cut");
        assert!(u.cuts() >= 1);
        assert!(f64::from(total) <= 0.85 * 64.0 * 40_000.0 + 1.0, "{total}");
        assert!((shares.iter().sum::<f64>() - f64::from(total)).abs() < 2.0);
    }

    /// ADR-8733: queues that a few peers see on their own downlinks never limit the shared uplink:
    /// three of eight peers behind slow downlinks (queues rising to 400 ms), and the join of the
    /// congested listen soak row (one delivering peer with a 190 ms queue, the next one joined but
    /// with nothing delivered yet), leave the aggregate unlimited and every share at its target.
    #[test]
    fn queues_a_few_peers_see_never_cut_the_aggregate() {
        let mut u = SharedUplink::new(0);
        let mut now = 0;
        for round in 0..40 {
            now += 50;
            let q = 20.0 * f64::from(round + 1).min(20.0);
            let peers: Vec<UplinkPeer> = (0..8)
                .map(|i| UplinkPeer {
                    target: 60_000.0,
                    delivery: 32_000.0,
                    queue_ms: if i < 3 { q } else { 0.0 },
                    weight: 1.0,
                    loss: 0.0,
                    starved: false,
                })
                .collect();
            let shares = u.update(now, &peers);
            assert!(shares.iter().all(|&s| s == 60_000.0), "{shares:?}");
        }
        assert_eq!(u.target_bytes_per_second(), None);
        assert_eq!(u.cuts(), 0);

        let mut join = SharedUplink::new(0);
        for (round, q) in [134.0, 190.0, 190.0, 356.0, 424.0].into_iter().enumerate() {
            let peers = [
                UplinkPeer {
                    target: 39_500.0,
                    delivery: 46_000.0,
                    queue_ms: q,
                    weight: 1.0,
                    loss: 0.0,
                    starved: false,
                },
                UplinkPeer {
                    target: 262_144.0,
                    delivery: 0.0,
                    queue_ms: 0.0,
                    weight: 1.0,
                    loss: 0.0,
                    starved: false,
                },
            ];
            join.update(66 * (round as u64 + 1), &peers);
        }
        assert_eq!(join.target_bytes_per_second(), None);
    }

    /// ADR-8733: the host signal is the median of the peers once enough delivering peers see a
    /// queue at once, and 0 below that; a peer that delivered nothing is no witness.
    #[test]
    fn the_host_queue_needs_enough_delivering_peers() {
        let peer = |queue_ms: f64, delivery: f64| UplinkPeer {
            target: 100_000.0,
            delivery,
            queue_ms,
            weight: 1.0,
            loss: 0.0,
            starved: false,
        };
        let three = [peer(60.0, 1.0), peer(60.0, 1.0), peer(60.0, 1.0), peer(0.0, 1.0)];
        assert_eq!(host_queue_ms(&three), 0.0);
        let four = [peer(60.0, 1.0), peer(50.0, 1.0), peer(40.0, 1.0), peer(30.0, 1.0)];
        assert_eq!(host_queue_ms(&four), 50.0);
        // Four queues, but two of the peers delivered nothing: three witnesses, under the bar.
        let idle = [
            peer(60.0, 1.0),
            peer(50.0, 1.0),
            peer(40.0, 0.0),
            peer(30.0, 0.0),
            peer(40.0, 1.0),
        ];
        assert_eq!(host_queue_ms(&idle), 0.0);
        // Most of eight peers at once: the median of the eight.
        let most: Vec<UplinkPeer> = (0..8)
            .map(|i| peer(if i < 5 { 40.0 } else { 0.0 }, 1.0))
            .collect();
        assert_eq!(host_queue_ms(&most), 40.0);
        assert_eq!(host_queue_ms(&[]), 0.0);
    }

    /// A host uplink as a fluid model, for the shared uplink's tests (ADR-10097): `peers` clients
    /// behind one drop-tail queue of 52 ms at `capacity` bytes per second. Each peer sends `fill`
    /// of what the shared uplink allows it (its demand while there is no target), and reads the
    /// host's queue, its loss and its delivery rate 100 ms late. A peer in `blind` joined over a
    /// standing queue: its minimum round trip sits on top of the queue, so it reads no delay.
    struct Host {
        uplink: SharedUplink,
        capacity: f64,
        demand: f64,
        fill: f64,
        peers: usize,
        blind: usize,
        peer_loss: Vec<f64>,
        queue: f64,
        shares: Vec<f64>,
        /// `(queue ms, loss, delivered fraction)` of the latest 100 ms.
        late: std::collections::VecDeque<(f64, f64, f64)>,
        /// Bytes the host offered in each whole second.
        offered: Vec<f64>,
        now: u64,
    }

    impl Host {
        const STEP_MS: u64 = 10;

        fn new(peers: usize, capacity: f64, demand: f64) -> Self {
            Self {
                uplink: SharedUplink::new(0),
                capacity,
                demand,
                fill: 1.0,
                peers,
                blind: 0,
                peer_loss: vec![0.0; peers],
                queue: 0.0,
                shares: vec![demand; peers],
                late: std::collections::VecDeque::new(),
                offered: Vec::new(),
                now: 0,
            }
        }

        fn run(&mut self, until_ms: u64) {
            let dt = Self::STEP_MS as f64 / 1_000.0;
            while self.now < until_ms {
                let offered: f64 = self.shares.iter().map(|s| s * self.fill).sum();
                let second = (self.now / 1_000) as usize;
                if self.offered.len() <= second {
                    self.offered.resize(second + 1, 0.0);
                }
                self.offered[second] += offered * dt;
                // The queue: 52 ms deep, drop-tail.
                let room = 0.052 * self.capacity;
                let arriving = self.queue + (offered - self.capacity) * dt;
                let dropped = (arriving - room).max(0.0);
                self.queue = arriving.clamp(0.0, room);
                let loss = dropped / (offered * dt).max(1.0);
                let busy = self.queue > 0.0 || offered >= self.capacity;
                let delivered = if busy { self.capacity } else { offered };
                self.late.push_back((
                    self.queue / self.capacity * 1_000.0,
                    loss,
                    delivered / offered.max(1.0),
                ));
                if self.late.len() > 10 {
                    self.late.pop_front();
                }
                let (queue_ms, loss, through) = self.late.front().copied().unwrap_or((0.0, 0.0, 1.0));
                let peers: Vec<UplinkPeer> = (0..self.peers)
                    .map(|i| UplinkPeer {
                        target: self.demand,
                        delivery: self.shares[i] * self.fill * through,
                        queue_ms: if i < self.blind { 0.0 } else { queue_ms },
                        weight: 1.0,
                        loss: loss.max(self.peer_loss[i]),
                        starved: false,
                    })
                    .collect();
                self.shares = self.uplink.update(self.now, &peers);
                self.now += Self::STEP_MS;
            }
        }

        /// The largest share of the uplink's rate a whole second from `from_s` on offered.
        fn worst_second(&self, from_s: usize) -> f64 {
            self.offered[from_s..]
                .iter()
                .fold(0.0f64, |worst, bytes| worst.max(bytes / self.capacity))
        }
    }

    /// KI-NT-031 (ADR-10097): a full listen server. 64 peers want twice what the host's
    /// 20 Mbit/s uplink carries. Once the aggregate has found the uplink's rate it stays under
    /// it: every 15 s a 150 ms probe asks whether the uplink got faster, meets the uplink's
    /// rate and leaves the estimate where it is. Before, the estimate was dropped after 15 s and
    /// the aggregate grew 8 % a second until a queue stood at the host: seconds over the uplink
    /// every 15 s.
    #[test]
    fn a_full_uplink_is_probed_without_a_second_over_its_rate() {
        let capacity = 2_500_000.0;
        let mut host = Host::new(64, capacity, 80_000.0);
        host.run(60_000);
        let worst = host.worst_second(5);
        let (probes, confirmed) = host.uplink.probes();
        let target = f64::from(host.uplink.target_bytes_per_second().expect("a target"));
        println!(
            "full uplink: worst second from 5 s {:.1} % of the uplink, {probes} probes ({confirmed} met its rate), target {:.1} %",
            worst * 100.0,
            target / capacity * 100.0
        );
        assert!(
            worst <= 1.0,
            "a second offered {:.1} % of the uplink's rate",
            worst * 100.0
        );
        assert!(
            probes >= 2 && confirmed == probes,
            "{probes} probes, {confirmed} confirmed"
        );
        assert!(
            target >= 0.9 * capacity,
            "the aggregate ends at {:.1} % of the uplink",
            target / capacity * 100.0
        );
    }

    /// ADR-10097: an uplink that gets faster is found. The host's uplink goes from 20 to
    /// 30 Mbit/s at 20 s; the probes after it meet no queue while the peers deliver more than the
    /// estimate, each raises the estimate to what they delivered, and by 60 s the aggregate uses
    /// the faster uplink, still without a second over it.
    #[test]
    fn a_faster_uplink_is_found_by_its_probes() {
        let mut host = Host::new(64, 2_500_000.0, 80_000.0);
        host.run(20_000);
        let before = host.worst_second(5);
        host.capacity = 3_750_000.0;
        host.run(60_000);
        let target = f64::from(host.uplink.target_bytes_per_second().expect("a target"));
        let (probes, confirmed) = host.uplink.probes();
        println!(
            "faster uplink: target {target:.0} B/s at 60 s, {probes} probes ({confirmed} met the uplink's rate)"
        );
        assert!(before <= 1.0, "{:.1} % before the change", before * 100.0);
        assert!(
            target >= 0.9 * 3_750_000.0,
            "the aggregate ends at {target:.0} B/s on a 3,750,000 B/s uplink"
        );
        let worst = host.offered[21..]
            .iter()
            .fold(0.0f64, |w, bytes| w.max(bytes / 3_750_000.0));
        assert!(
            worst <= 1.0,
            "a second offered {:.1} % of the faster uplink",
            worst * 100.0
        );
    }

    /// ADR-10097: the start of a full listen server. 40 of 64 peers joined while the host's
    /// queue already stood, so they read no queueing delay and the median peer reads none: the
    /// queue signal is blind while the host offers 28 % more than its uplink carries and the
    /// uplink drops a fifth of every peer's packets. The peers' loss gives the aggregate its
    /// first target within a second. Before, it never took one.
    #[test]
    fn a_host_that_drops_takes_its_first_target_from_its_peers_loss() {
        let capacity = 2_500_000.0;
        let mut host = Host::new(64, capacity, 50_000.0);
        host.blind = 40;
        host.run(1_000);
        assert!(
            host.uplink.target_bytes_per_second().is_some(),
            "no target after a second of drops"
        );
        host.run(10_000);
        let worst = host.worst_second(3);
        println!(
            "drops: worst second from 3 s {:.1} % of the uplink",
            worst * 100.0
        );
        assert!(
            worst <= 1.0,
            "a second offered {:.1} % of the uplink's rate",
            worst * 100.0
        );
    }

    /// ADR-10097: loss on clients' own links gives the aggregate no target. 30 of 64 peers lose
    /// a fifth of their packets on their own links (the median peer loses none), and on a small
    /// server all 12 peers do (under [`UPLINK_CROWD_PEERS`] the peers' own controllers answer).
    #[test]
    fn loss_on_the_clients_own_links_gives_no_target() {
        let mut some = Host::new(64, 10_000_000.0, 50_000.0);
        for loss in some.peer_loss.iter_mut().take(30) {
            *loss = 0.2;
        }
        some.run(10_000);
        assert_eq!(some.uplink.target_bytes_per_second(), None);
        let mut small = Host::new(12, 10_000_000.0, 50_000.0);
        small.peer_loss = vec![0.3; 12];
        small.run(10_000);
        assert_eq!(small.uplink.target_bytes_per_second(), None);
    }

    /// ADR-10097: a hold never takes the aggregate under the uplink's measured rate. The peers
    /// leave 7 % of their shares unused, and the median queueing delay wobbles between 5 and
    /// 7 ms while a few peers read 30 ms on links of their own: every rise reads as a queue
    /// building at the host. Each such hold used to set the aggregate to 97 % of what the peers
    /// delivered, which was already less than the aggregate: step by step down, to a quarter of
    /// the uplink in 14 s on listen-uplink/64 seed 177.
    #[test]
    fn a_hold_keeps_the_uplinks_measured_rate() {
        let capacity = 2_500_000.0;
        let mut u = SharedUplink::new(0);
        let peers = |queue_ms: f64, shares: &[f64], fill: f64| -> Vec<UplinkPeer> {
            (0..64)
                .map(|i| UplinkPeer {
                    target: 80_000.0,
                    delivery: shares.get(i).copied().unwrap_or(capacity / 64.0) * fill,
                    queue_ms: if i < 6 { 30.0 } else { queue_ms },
                    weight: 1.0,
                    loss: 0.0,
                    starved: false,
                })
                .collect()
        };
        // A queue builds at the host while the peers deliver the uplink's rate.
        let mut now = 0;
        let mut shares = Vec::new();
        for q in [0.0, 0.0, 6.0, 8.0, 10.0, 12.0] {
            now += 50;
            shares = u.update(now, &peers(q, &[], 1.0));
        }
        let held = f64::from(u.target_bytes_per_second().expect("held"));
        assert!((held - BOTTLENECK_HEADROOM * capacity).abs() < 1_000.0, "{held}");
        // Ten seconds of wobble with part of every share unused.
        for round in 0..200 {
            now += 50;
            let q = if round % 4 < 2 { 5.0 } else { 7.0 };
            shares = u.update(now, &peers(q, &shares, 0.93));
        }
        let target = f64::from(u.target_bytes_per_second().expect("a target"));
        println!(
            "wobble: the aggregate ends at {:.1} % of the uplink's rate",
            target / capacity * 100.0
        );
        assert!(
            target >= 0.96 * capacity,
            "the aggregate fell to {:.1} % of the uplink's rate",
            target / capacity * 100.0
        );
    }

    /// ADR-10097: under a crowd ([`UPLINK_CROWD_PEERS`]) the aggregate keeps its earlier form.
    /// Ten peers; four read 30 ms on links of their own and the median wobbles between 5 and
    /// 7 ms, so every rise reads as a queue building at the host. Each hold takes the aggregate to
    /// 97 % of what the peers deliver right then (the estimate follows them down, where a crowd's
    /// estimate only rises), no probe starts, and after [`BOTTLENECK_HOLD_MS`] without a queue
    /// the estimate is dropped.
    #[test]
    fn a_few_peers_keep_the_cautious_hold_and_never_probe() {
        let mut u = SharedUplink::new(0);
        let peers = |queue_ms: f64, delivery: f64| -> Vec<UplinkPeer> {
            (0..10)
                .map(|i| UplinkPeer {
                    target: 40_000.0,
                    delivery,
                    queue_ms: if i < 4 { 30.0 } else { queue_ms },
                    weight: 1.0,
                    loss: 0.0,
                    starved: false,
                })
                .collect()
        };
        let mut now = 0;
        for q in [0.0, 0.0, 6.0, 8.0] {
            now += 50;
            u.update(now, &peers(q, 30_000.0));
        }
        assert_eq!(u.bottleneck_bytes_per_second(), Some(300_000.0));
        let first = f64::from(u.target_bytes_per_second().expect("held"));
        assert!((first - BOTTLENECK_HEADROOM * 300_000.0).abs() < 1.0, "{first}");
        // The peers deliver a third less; the wobble goes on.
        for round in 0..40 {
            now += 50;
            let q = if round % 4 < 2 { 5.0 } else { 7.0 };
            u.update(now, &peers(q, 20_000.0));
        }
        assert_eq!(u.bottleneck_bytes_per_second(), Some(200_000.0));
        let second = f64::from(u.target_bytes_per_second().expect("held"));
        assert!((second - BOTTLENECK_HEADROOM * 200_000.0).abs() < 1.0, "{second}");
        assert_eq!(u.cuts(), 0, "holds, not cuts");
        // No queue for a hold: the estimate is dropped, and no probe ever started.
        for _ in 0..(BOTTLENECK_HOLD_MS / 50 + 2) {
            now += 50;
            u.update(now, &peers(0.0, 20_000.0));
        }
        assert_eq!(u.bottleneck_bytes_per_second(), None);
        assert_eq!(u.probes(), (0, 0));
    }

    /// ADR-10097: the start of a full listen server. A drop-tail queue that is full takes the
    /// head of every flush and drops its tail: eight of 64 peers get nothing through and still
    /// talk, the other 56 lose nothing and, having joined over the standing queue, read no
    /// delay. Neither the median queue nor the median loss shows it; the starved peers give the
    /// aggregate its first target at once. Three starved peers do not (clients whose own
    /// downlinks died), and neither do starved peers of a small server.
    #[test]
    fn peers_the_uplink_starves_give_it_its_first_target() {
        let peers = |n: usize, starved: usize| -> Vec<UplinkPeer> {
            (0..n)
                .map(|i| UplinkPeer {
                    target: 120_000.0,
                    delivery: if i < starved { 0.0 } else { 44_000.0 },
                    queue_ms: 0.0,
                    weight: 1.0,
                    loss: 0.0,
                    starved: i < starved,
                })
                .collect()
        };
        let run = |n: usize, starved: usize, rounds: u64| {
            let mut u = SharedUplink::new(0);
            let mut shares = Vec::new();
            for round in 1..=rounds {
                shares = u.update(round * 50, &peers(n, starved));
            }
            (
                u.target_bytes_per_second(),
                u.bottleneck_bytes_per_second(),
                shares,
            )
        };
        let (target, rate, shares) = run(64, 8, 1);
        let carried = 56.0 * 44_000.0;
        assert_eq!(rate, Some(carried));
        let target = f64::from(target.expect("a first target"));
        assert!((target - DECREASE_FACTOR * carried).abs() < 1.0, "{target}");
        // The starved peers get a fair share of it like everybody else.
        assert!((shares[0] - target / 64.0).abs() < 1.0, "{}", shares[0]);
        assert_eq!(
            run(64, 3, 20).0,
            None,
            "three starved peers are not the host's uplink"
        );
        assert_eq!(
            run(14, 6, 20).0,
            None,
            "under a crowd the peers' own controllers answer"
        );
    }

    /// ADR-10097 (KI-NT-032): what packets cost on the wire per body byte. 93-byte bodies behind
    /// a 34-byte endpoint header and 28 bytes of IP and UDP headers cost 155 / 93; full packets
    /// cost little over 1; the figure follows a change of packet size within a few seconds.
    #[test]
    fn the_wire_share_follows_the_packet_sizes() {
        let mut w = WireShare::default();
        assert_eq!(w.ratio(), 1.0);
        let mut now = 0;
        for _ in 0..200 {
            now += 16;
            w.sent(now, 93, 34);
        }
        assert!((w.ratio() - 155.0 / 93.0).abs() < 1e-9, "{}", w.ratio());
        for _ in 0..400 {
            now += 16;
            w.sent(now, 1_200, 34);
        }
        let full = (1_200.0 + 34.0 + 28.0) / 1_200.0;
        assert!((w.ratio() - full).abs() < 0.005, "{} against {full}", w.ratio());
        // Idle for a minute, then one small packet: the old packets weigh little by then, and the
        // figure moves toward the new size.
        let before = w.ratio();
        w.sent(now + 60_000, 93, 34);
        assert!(w.ratio() > before && w.ratio() <= 155.0 / 93.0, "{}", w.ratio());
    }

    /// Four of eight peers on their own slow downlinks are not the host uplink. The other four
    /// see no queue, so the host's modem is not delaying everyone. `host_queue_ms` still returns
    /// the upper median (index `len/2` of `[0,0,0,0,400,400,400,400]` is 400) once four
    /// delivering peers clear the witness gate, and the aggregate then cuts for the peers that
    /// are not queued. One machine can be those four peers: `max_peers_per_ip` is 8.
    #[test]
    fn half_the_peers_on_slow_downlinks_do_not_cut_the_shared_uplink() {
        let slow = |queue_ms: f64| UplinkPeer {
            target: 60_000.0,
            delivery: 32_000.0,
            queue_ms,
            weight: 1.0,
            loss: 0.0,
            starved: false,
        };
        let split: Vec<UplinkPeer> = (0..8).map(|i| slow(if i < 4 { 400.0 } else { 0.0 })).collect();
        assert_eq!(
            host_queue_ms(&split),
            0.0,
            "upper median of a 4/8 split is the slow half's own downlink, not the host"
        );

        let mut u = SharedUplink::new(0);
        let mut now = 0;
        for round in 0..40 {
            now += 50;
            let q = 20.0 * f64::from(round + 1).min(20.0);
            let peers: Vec<UplinkPeer> = (0..8)
                .map(|i| UplinkPeer {
                    target: 60_000.0,
                    delivery: 32_000.0,
                    queue_ms: if i < 4 { q } else { 0.0 },
                    weight: 1.0,
                    loss: 0.0,
                    starved: false,
                })
                .collect();
            let shares = u.update(now, &peers);
            assert!(
                shares.iter().all(|&s| s == 60_000.0),
                "round {round} cut the uncongested half: {shares:?}"
            );
        }
        assert_eq!(u.target_bytes_per_second(), None);
        assert_eq!(u.cuts(), 0);
    }

    /// ADR-8883: the connections of one machine are one witness of the host queue. Five of eight
    /// peers see a queue, but four of them share one address: two witness blocks of five, no
    /// majority, and the shared uplink never cuts. The same queues from five machines are the
    /// host's uplink and cut it.
    #[test]
    fn one_machines_connections_are_one_witness_of_the_host_queue() {
        use crate::addr_block::AddrBlock;
        let block = |last: u8| AddrBlock::of(std::net::IpAddr::from([198, 51, 100, last]));
        let peers_at = |q: f64| -> Vec<UplinkPeer> {
            (0..8)
                .map(|i| UplinkPeer {
                    target: 60_000.0,
                    delivery: 32_000.0,
                    queue_ms: if i < 5 { q } else { 0.0 },
                    weight: 1.0,
                    loss: 0.0,
                    starved: false,
                })
                .collect()
        };
        // Peers 0..4 are one machine; 4..8 one machine each.
        let shared: Vec<AddrBlock> = (0..8u8).map(|i| block(if i < 4 { 1 } else { 10 + i })).collect();
        let distinct: Vec<AddrBlock> = (0..8u8).map(|i| block(10 + i)).collect();
        let peers = peers_at(400.0);
        assert_eq!(
            host_queue_ms(&peers),
            400.0,
            "per peer, five of eight is a majority"
        );
        assert_eq!(host_queue_ms_by_block(&peers, &shared), 0.0);
        assert_eq!(host_queue_ms_by_block(&peers, &distinct), 400.0);
        // A block's witness is its largest delivering queue, and its delivery the sum.
        // Four machines, each one delivering peer at 60 ms and one idle peer at 900 ms: the
        // idle peers are no witnesses, so each block reads 60.
        let pairs: Vec<UplinkPeer> = (0..8)
            .map(|i| UplinkPeer {
                target: 60_000.0,
                delivery: if i % 2 == 0 { 32_000.0 } else { 0.0 },
                queue_ms: if i % 2 == 0 { 60.0 } else { 900.0 },
                weight: 1.0,
                loss: 0.0,
                starved: false,
            })
            .collect();
        let pair_blocks: Vec<AddrBlock> = (0..8u8).map(|i| block(20 + i / 2)).collect();
        assert_eq!(host_queue_ms_by_block(&pairs, &pair_blocks), 60.0);
        // A block list that does not match the peers reads every peer as its own block.
        assert_eq!(
            host_queue_ms_by_block(&peers, &distinct[..7]),
            host_queue_ms(&peers)
        );

        let mut one_machine = SharedUplink::new(0);
        let mut five_machines = SharedUplink::new(0);
        for round in 0..40u32 {
            let now = 50 * u64::from(round + 1);
            let peers = peers_at(20.0 * f64::from(round + 1).min(20.0));
            let shares = one_machine.update_by_block(now, &peers, &shared);
            assert!(
                shares.iter().all(|&s| s == 60_000.0),
                "round {round}: one machine cut everyone: {shares:?}"
            );
            five_machines.update_by_block(now, &peers, &distinct);
        }
        assert_eq!(one_machine.cuts(), 0);
        assert!(
            five_machines.cuts() > 0,
            "a majority of machines is the host uplink"
        );
    }

    /// The trendline filter flags a one-way delay that grows with every group and stays normal on a
    /// flat path.
    #[test]
    fn the_trendline_filter_sees_a_growing_delay() {
        let mut flat = TrendlineFilter::new();
        let mut growing = TrendlineFilter::new();
        for i in 0..200 {
            let send = f64::from(i) * 10.0;
            flat.add(send, send + 20.0 + f64::from(i % 3));
            growing.add(send, send + 20.0 + f64::from(i) * 2.0);
        }
        assert_eq!(flat.signal(), GradientSignal::Normal);
        assert_eq!(growing.signal(), GradientSignal::Overuse);
    }

    /// ADR-8534: the essential class (lockstep frames, inputs) leaves inside its floor whatever
    /// the target says, so the target never falls under its rate plus
    /// [`ESSENTIAL_RESERVE_BYTES_PER_SECOND`]. An app-limited host sending 15 kB/s of essential
    /// frames on a jittery path: rising round trips (overuse) and rounds that acknowledged a
    /// single packet would cut it to 0.85 x one round's delivery, about 3.4 kB/s (measured on the
    /// soak's bad-wifi seat: 256 KiB/s to 4.6 kB/s in one cut, under 12-15 kB/s of frames), and
    /// the reliable stream behind the frames got no allowance at all.
    #[test]
    fn a_cut_never_takes_the_target_under_the_essential_class() {
        let mut c = RateController::new(RateConfig::default());
        let mut lowest = u32::MAX;
        for i in 0..200u64 {
            let now = 50 * (i + 1);
            // Every fourth round acknowledges one small packet only; the round trip climbs.
            let thin = i % 4 == 3;
            c.on_round(
                now,
                &RoundSample {
                    interval_ms: 50,
                    acked_bytes: if thin { 200 } else { 800 },
                    sent_bytes: 800,
                    acked_packets: if thin { 1 } else { 4 },
                    lost_packets: 0,
                    round_min_rtt_ms: 40.0 + (i % 8) as f64 * 12.0,
                    srtt_ms: 60.0,
                    min_rtt_ms: 40.0,
                    delay_samples: 0,
                    round_min_one_way_ms: 0.0,
                    min_one_way_ms: 0.0,
                    gradient: None,
                    held_bytes: 0,
                    capped: false,
                    essential_bytes: 750,
                },
            );
            if i >= 20 {
                lowest = lowest.min(c.target_bytes_per_second());
            }
        }
        assert!(c.cuts() > 0, "the rising round trips cut");
        let essential = c.essential_bytes_per_second();
        assert!((14_000.0..=15_000.0).contains(&essential), "{essential:.0}");
        let floor = 15_000 + ESSENTIAL_RESERVE_BYTES_PER_SECOND - 1_000;
        assert!(lowest >= floor, "lowest target {lowest} under {floor}");
    }

    /// One round of [`a_congestion_cuts_recovery_stops_at_the_headroom`]: a saturating sender on
    /// a 32 kB/s path in 100 ms rounds; it delivers at most the path's rate and loses `loss` of
    /// what it offered; `queue_ms` over the 60 ms minimum round trip.
    fn path_round(c: &mut RateController, now: u64, loss: f64, queue_ms: f64) -> f64 {
        let offered = f64::from(c.target_bytes_per_second()) * 0.1;
        let packets = (offered / 1_000.0).ceil().max(1.0) as u32;
        let lost = (f64::from(packets) * loss).round() as u32;
        c.on_round(
            now,
            &RoundSample {
                interval_ms: 100,
                acked_bytes: (offered * (1.0 - loss)).min(3_200.0) as u64,
                sent_bytes: offered as u64,
                acked_packets: packets - lost,
                lost_packets: lost,
                round_min_rtt_ms: 60.0 + queue_ms,
                srtt_ms: 60.0 + queue_ms,
                min_rtt_ms: 60.0,
                delay_samples: 0,
                round_min_one_way_ms: 0.0,
                min_one_way_ms: 0.0,
                gradient: None,
                held_bytes: 1,
                capped: false,
                essential_bytes: 0,
            },
        );
        offered * 10.0
    }

    /// ADR-8534: a congestion cut's recovery (item 5) owes its pre-cut rate only as far as the
    /// headroom under the rate the path delivered at the cut. A 40 kB/s sender on a 32 kB/s
    /// path: loss with a standing queue (a full drop-tail buffer) cuts it; the loss clears while
    /// a small queue stays (the path is still full near its rate). Before ADR-8534 the recovery
    /// went back to 40 kB/s within its 2 s (the soak's congested row, seed 15: 34 kB/s offered for
    /// a second on its 32 kB/s link); now it stays at 97 % of the 32 kB/s it measured.
    #[test]
    fn a_congestion_cuts_recovery_stops_at_the_headroom() {
        let mut c = RateController::new(RateConfig {
            start_bytes_per_second: 40_000,
            ..blind(RateConfig::default())
        });
        let mut now = 0;
        for _ in 0..20 {
            now += 100;
            path_round(&mut c, now, 0.15, 30.0);
        }
        assert!(c.cuts() > 0, "the congestive loss cut");
        let mut highest = 0.0f64;
        for _ in 0..50 {
            now += 100;
            highest = highest.max(path_round(&mut c, now, 0.0, 8.0));
        }
        assert!(
            highest <= BOTTLENECK_HEADROOM * 32_000.0 + 1.0,
            "offered {highest:.0} after the loss cleared, path 32,000"
        );
        assert!(highest >= 0.9 * 32_000.0, "recovered to {highest:.0}");
    }

    /// The same episode on a path that shows no queue once the loss clears (DO27 A4's transient
    /// loss): RECOVERY_CLEAN_ROUNDS rounds at the headroom with no queue owe the rest at once, and
    /// the sender is back at its 40 kB/s within the item's 5 s.
    #[test]
    fn a_transient_congestion_cut_still_recovers_in_full() {
        let mut c = RateController::new(RateConfig {
            start_bytes_per_second: 40_000,
            ..blind(RateConfig::default())
        });
        let mut now = 0;
        for _ in 0..20 {
            now += 100;
            path_round(&mut c, now, 0.15, 30.0);
        }
        let mut back = None;
        for i in 1..=50u64 {
            now += 100;
            // The path has room again: everything offered is delivered, no queue.
            let offered = f64::from(c.target_bytes_per_second()) * 0.1;
            c.on_round(
                now,
                &RoundSample {
                    interval_ms: 100,
                    acked_bytes: offered as u64,
                    sent_bytes: offered as u64,
                    acked_packets: (offered / 1_000.0).ceil().max(1.0) as u32,
                    lost_packets: 0,
                    round_min_rtt_ms: 60.0,
                    srtt_ms: 60.0,
                    min_rtt_ms: 60.0,
                    delay_samples: 0,
                    round_min_one_way_ms: 0.0,
                    min_one_way_ms: 0.0,
                    gradient: None,
                    held_bytes: 1,
                    capped: false,
                    essential_bytes: 0,
                },
            );
            if back.is_none() && c.target_bytes_per_second() >= 40_000 {
                back = Some(i * 100);
            }
        }
        let back = back.expect("back at the pre-cut rate");
        assert!(back <= 5_000, "back after {back} ms");
    }

    /// ADR-8534: [`Pacer::limit`] withdraws a first burst above the depth.
    #[test]
    fn a_limited_pacer_keeps_no_first_burst() {
        let mut p = Pacer::new(76_800);
        p.refill(0, 10_000.0, 2_400.0);
        p.limit(2_400.0);
        p.spend(2_400);
        assert!(!p.ready(), "only the depth was left");
    }

    /// ADR-10063: a bucket still holding its first burst above the depth reports as unused only
    /// what an interval earned beyond what the sender spent in it. A sender that spends more than
    /// the target while it drains the initial window earns nothing; one that spends less earns
    /// the difference, as below the depth.
    #[test]
    fn a_first_burst_above_the_depth_is_no_unused_allowance() {
        let (rate, depth) = (16_000.0, 4_800.0);
        let mut p = Pacer::with_gain(76_800, 1.0);
        assert_eq!(p.refill_overflow(0, rate, depth), 0.0);
        p.spend(41_000);
        let mut unused = 0.0;
        for now in (16..2_000).step_by(16) {
            unused += p.refill_overflow(now, rate, depth);
            // 340 B every 16 ms: 21 kB/s against a 16 kB/s target.
            p.spend(340);
        }
        assert_eq!(unused, 0.0, "a saturating sender earned {unused}");
        let mut q = Pacer::with_gain(76_800, 1.0);
        let _ = q.refill_overflow(0, rate, depth);
        q.spend(128);
        // 16 ms earn 256 B; 128 B were spent.
        let unused = q.refill_overflow(16, rate, depth);
        assert!((unused - 128.0).abs() < 1e-9, "{unused}");
    }

    /// One 110 ms round of a saturating sender at `rate` on a path with a 90 ms minimum round
    /// trip that loses about 1 packet in 13 (lossy, under the 10 % cut): `acked` packets, one
    /// lost, the round's smallest round trip `round_min`.
    fn lossy_round(rate: f64, acked: u32, round_min: f64) -> RoundSample {
        let sent = (rate * 0.11) as u64;
        RoundSample {
            interval_ms: 110,
            acked_bytes: sent * u64::from(acked) / u64::from(acked + 1),
            sent_bytes: sent,
            acked_packets: acked,
            lost_packets: 1,
            round_min_rtt_ms: round_min,
            srtt_ms: 100.0,
            min_rtt_ms: 90.0,
            delay_samples: 0,
            round_min_one_way_ms: 0.0,
            min_one_way_ms: 0.0,
            gradient: None,
            held_bytes: 1,
            capped: false,
            essential_bytes: 0,
        }
    }

    /// ADR-8658: on a lossy, jittery path a round acknowledges a handful of packets, and the
    /// smallest of them climbs by the jitter alone. Four thin rounds whose smallest round trips
    /// read 100, 110, 120 and 130 ms (over a 90 ms minimum that the rounds before them met) are no
    /// growing queue: the gradient reads back over the same packets the threshold detector does
    /// (ADR-8490). With the floor of two thin rounds, they cut a 240 kB/s sender to 85 % of one
    /// thin round's delivery, and the bottleneck estimate measured at the cut held it there.
    #[test]
    fn a_lossy_paths_thin_rounds_climbing_by_jitter_are_no_gradient() {
        let mut c = RateController::new(RateConfig {
            max_bytes_per_second: 240_000,
            ..RateConfig::client()
        });
        let mut now = 0;
        // Out of the startup, at the cap, with the path's minimum met in every round.
        while now < 5_000 {
            now += 110;
            let rate = f64::from(c.target_bytes_per_second());
            c.on_round(now, &lossy_round(rate, 20, 90.0));
        }
        let (before, cuts) = (c.target_bytes_per_second(), c.cuts());
        assert!(before >= 200_000, "{before}");
        for (acked, round_min) in [(12, 100.0), (12, 110.0), (19, 120.0), (9, 130.0)] {
            now += 110;
            let rate = f64::from(c.target_bytes_per_second());
            c.on_round(now, &lossy_round(rate, acked, round_min));
        }
        assert_eq!(c.cuts(), cuts, "jitter in thin rounds is no queue");
        assert!(c.target_bytes_per_second() >= before);
    }

    /// ADR-8658: the reach-back still sees a real queue on the same lossy path. A queue that
    /// grows 10 ms per round delays every packet of every round: the sender is cut within ten
    /// rounds.
    #[test]
    fn a_lossy_paths_growing_queue_still_cuts() {
        let mut c = RateController::new(RateConfig {
            max_bytes_per_second: 240_000,
            ..RateConfig::client()
        });
        let mut now = 0;
        while now < 5_000 {
            now += 110;
            let rate = f64::from(c.target_bytes_per_second());
            c.on_round(now, &lossy_round(rate, 20, 90.0));
        }
        let (before, cuts) = (c.target_bytes_per_second(), c.cuts());
        let start = now;
        let mut cut_at = None;
        for k in 1..=10u32 {
            now += 110;
            let rate = f64::from(c.target_bytes_per_second());
            c.on_round(now, &lossy_round(rate, 12, 90.0 + 10.0 * f64::from(k)));
            if cut_at.is_none() && c.cuts() > cuts {
                cut_at = Some(now - start);
            }
        }
        let cut_at = cut_at.expect("a growing queue is cut");
        assert!(cut_at <= 1_100, "cut after {cut_at} ms");
        assert!(c.target_bytes_per_second() < before);
    }

    /// ADR-10050: the stamps are 16 bits of the peer's millisecond clock, which shares no origin
    /// with this side's. A packet a tick over 100 s on a 40 ms path (the stamp wraps once): every
    /// sample is the same, so the queue reads 0; 30 ms more on the way out reads 30 ms, wherever
    /// the peer's clock started.
    #[test]
    fn one_way_delay_needs_no_common_clock_and_unwraps_its_stamps() {
        for peer_origin in [0u64, 65_500, 7_654_321, 0xffff_ffff_0000] {
            let mut d = OneWayDelay::new();
            assert_eq!(d.min_ms(), 0.0, "no sample yet");
            let mut sent = 5_000u64;
            while sent < 105_000 {
                let arrival = sent + 40;
                // The frame comes back 55 ms after the packet left.
                let sample = d.sample(sent + 55, sent, (peer_origin + arrival) as u16);
                assert!(sample > 0.0);
                assert_eq!(d.queue_ms(), 0.0, "origin {peer_origin}, sent {sent}");
                sent += 16;
            }
            let arrival = sent + 40 + 30;
            d.sample(sent + 85, sent, (peer_origin + arrival) as u16);
            assert_eq!(d.queue_ms(), 30.0, "origin {peer_origin}");
        }
    }

    /// ADR-10050: the minimum is the smallest sample of the last `ONE_WAY_WINDOW_MS`, so a path
    /// that got slower for good is the new base after that long; and a silence longer than
    /// `ONE_WAY_RESTART_MS` (a stamp could have wrapped unseen) starts a new measurement instead
    /// of comparing with the old one.
    #[test]
    fn one_way_delay_forgets_an_old_minimum_and_restarts_after_a_silence() {
        let mut d = OneWayDelay::new();
        let mut sent = 0u64;
        while sent < 2_000 {
            d.sample(sent + 50, sent, (sent + 40) as u16);
            sent += 20;
        }
        // The path is 25 ms slower from here on.
        while sent < 1_800 + ONE_WAY_WINDOW_MS {
            d.sample(sent + 75, sent, (sent + 65) as u16);
            assert_eq!(d.queue_ms(), 25.0, "at {sent}");
            sent += 20;
        }
        sent += 400;
        d.sample(sent + 75, sent, (sent + 65) as u16);
        assert_eq!(d.queue_ms(), 0.0, "the old minimum left the window");
        assert_eq!(d.min_ms(), ONE_WAY_ORIGIN_MS + 25.0);
        // 70 s of silence: the peer's clock went on (here it also jumped), the stamp wrapped.
        sent += 70_000;
        d.sample(sent + 75, sent, (sent + 12_345) as u16);
        assert_eq!(d.queue_ms(), 0.0, "a new measurement");
        assert_eq!(
            d.min_ms(),
            ONE_WAY_ORIGIN_MS,
            "its first sample sits at the scale's origin"
        );
    }

    /// A round of a sender whose target is `target`: it sent and had acknowledged `used` of it
    /// (a fraction), its round trips read `rtt_queue` ms over a 48 ms minimum, and its one-way
    /// delay, in `delay_samples` samples, reads `one_way_queue` ms over its own minimum (`None`:
    /// a peer that sends no arrival stamp).
    fn two_readings(
        target: f64,
        used: f64,
        rtt_queue: f64,
        one_way_queue: Option<f64>,
        delay_samples: u32,
    ) -> RoundSample {
        RoundSample {
            interval_ms: 80,
            acked_bytes: (target * used * 0.08) as u64,
            sent_bytes: (target * used * 0.08) as u64,
            acked_packets: 6,
            lost_packets: 0,
            round_min_rtt_ms: 48.0 + rtt_queue,
            srtt_ms: 80.0,
            min_rtt_ms: 48.0,
            delay_samples,
            round_min_one_way_ms: one_way_queue.map_or(0.0, |q| ONE_WAY_ORIGIN_MS + q),
            min_one_way_ms: one_way_queue.map_or(0.0, |_| ONE_WAY_ORIGIN_MS),
            gradient: None,
            held_bytes: 0,
            capped: false,
            essential_bytes: 0,
        }
    }

    /// (cuts, unconfirmed verdicts) of a controller fed 10 s of rounds of a sender that uses
    /// `used` of its target, with `delay_samples` samples a round whose readings follow
    /// `readings(round)`: (round-trip queue, one-way queue).
    fn cuts_over(used: f64, delay_samples: u32, readings: impl Fn(u64) -> (f64, Option<f64>)) -> (u64, u64) {
        let mut c = RateController::new(RateConfig {
            max_bytes_per_second: 30_000,
            ..RateConfig::client()
        });
        let mut now = 0;
        for k in 0..125u64 {
            now += 80;
            let (rtt_queue, one_way_queue) = readings(k);
            let target = f64::from(c.target_bytes_per_second());
            c.on_round(
                now,
                &two_readings(target, used, rtt_queue, one_way_queue, delay_samples),
            );
        }
        (c.cuts(), c.unconfirmed_overuse())
    }

    /// The round trips of ADR-8692's client: 32 ms over a minimum that a small
    /// acknowledgment-only packet set on a slow return path, moving by a tick with the phase of
    /// the socket reads (32, 48, 32, 48 ms ...).
    fn slow_return_path(k: u64) -> f64 {
        32.0 + 16.0 * (k % 2) as f64
    }

    /// ADR-10050, the up-leg cause of ADR-8692 at the controller. Read alone the round trips are
    /// a queue over the 25 ms threshold that rises again and again: a cut every few rounds, of a
    /// sender that uses a fifth of its target on an idle way out. Its own packets arrive with
    /// the same one-way delay every time, so the verdicts are not confirmed and nothing is cut.
    #[test]
    fn an_app_limited_senders_slow_return_path_is_not_cut() {
        let (cuts, _) = cuts_over(0.2, 8, |k| (slow_return_path(k), None));
        assert!(cuts >= 10, "the control (no arrival stamps): {cuts} cuts");
        let (cuts, unconfirmed) = cuts_over(0.2, 8, |k| (slow_return_path(k), Some(0.0)));
        assert_eq!(cuts, 0, "cuts with a flat one-way delay");
        assert!(unconfirmed >= 10, "{unconfirmed} verdicts dropped");
        // Read-phase noise on the way out, under the confirming delay, confirms nothing either.
        let (cuts, _) = cuts_over(0.2, 8, |k| (slow_return_path(k), Some(3.0 * (k % 4) as f64)));
        assert_eq!(cuts, 0);
    }

    /// ADR-10050: the check is for app-limited senders only. A sender that fills its target is
    /// read exactly as before wire 6, whatever its one-way delay says: its early cuts are what
    /// keeps its seconds under a bottleneck's rate (soak `congested`: with every verdict
    /// checked, 1 seed of 200 had a second over the link and none had before).
    #[test]
    fn a_sender_that_fills_its_target_is_read_as_before() {
        let (without, _) = cuts_over(1.0, 8, |k| (slow_return_path(k), None));
        let (with, unconfirmed) = cuts_over(1.0, 8, |k| (slow_return_path(k), Some(0.0)));
        assert!(without >= 10, "{without} cuts");
        assert_eq!(with, without, "the same cuts with a flat one-way delay");
        assert_eq!(unconfirmed, 0);
    }

    /// ADR-10050: the peers of a sender that splits a shared uplink are read as before as well
    /// (`set_shared_uplink`): each is app-limited against its own target while together they
    /// fill the host's uplink.
    #[test]
    fn a_shared_uplinks_peers_are_read_as_before() {
        let run = |one_way: Option<f64>| {
            let mut c = RateController::new(RateConfig {
                max_bytes_per_second: 30_000,
                ..RateConfig::client()
            });
            c.set_shared_uplink(true);
            let mut now = 0;
            for k in 0..125u64 {
                now += 80;
                let target = f64::from(c.target_bytes_per_second());
                c.on_round(now, &two_readings(target, 0.2, slow_return_path(k), one_way, 8));
            }
            (c.cuts(), c.unconfirmed_overuse())
        };
        let (without, _) = run(None);
        let (with, unconfirmed) = run(Some(0.0));
        assert!(without >= 10, "{without} cuts");
        assert_eq!(with, without, "the same cuts with a flat one-way delay");
        assert_eq!(unconfirmed, 0);
    }

    /// ADR-10050: an app-limited sender's queue on the way out is still cut. The queue grows
    /// 6 ms a round from round 20 to round 40, then stands; it shows in both readings, with the
    /// return path's 32 ms on top of the round trips or without.
    #[test]
    fn an_app_limited_senders_queue_on_the_way_out_still_cuts() {
        let queue = |k: u64| 6.0 * k.clamp(20, 40).saturating_sub(20) as f64;
        for return_ms in [0.0, 32.0] {
            let (cuts, _) = cuts_over(0.2, 8, |k| (queue(k) + return_ms, Some(queue(k))));
            assert!(cuts >= 1, "no cut with {return_ms} ms of return path");
        }
    }

    /// ADR-10050: the confirming reading spans `ONE_WAY_CONFIRM_SAMPLES` samples. A thin
    /// sender's rounds hold one sample each, and a single sample carries the receiver's read
    /// phase: here 3 to 25 ms in a fixed pattern (3, 20, 14, 25, 18, 22 ms), so two rounds in a
    /// row are both over the confirming delay again and again, while the smallest of any six in
    /// a row is 3 ms. With six samples in every round the same values are six real readings of
    /// a queue, and the verdicts stand (the control). The pattern starts low: before six samples
    /// exist the reading is of the ones there are.
    #[test]
    fn the_confirming_delay_is_read_over_enough_samples() {
        let phase = |k: u64| [3.0, 20.0, 14.0, 25.0, 18.0, 22.0][(k % 6) as usize];
        let readings = |k: u64| (slow_return_path(k), Some(phase(k)));
        let (cuts, _) = cuts_over(0.2, ONE_WAY_CONFIRM_SAMPLES, readings);
        assert!(
            cuts >= 5,
            "the control: {cuts} cuts when every round holds enough samples"
        );
        let (cuts, unconfirmed) = cuts_over(0.2, 1, readings);
        assert_eq!(cuts, 0, "cuts of a sender with one sample a round");
        assert!(unconfirmed >= 10, "{unconfirmed} verdicts dropped");
    }

    /// ADR-10050: the one-way delay also carries the two clocks' rate difference. A peer whose
    /// clock runs 0.5 % fast (the soak's clock-drift profile) stamps 0.4 ms later every 80 ms
    /// round, so the reading passes the confirming delay within 3 s and stays there: the check
    /// then confirms everything and the sender is read from its round trips alone, as before
    /// wire 6. A slow clock makes every sample the new minimum (a reading of 0).
    #[test]
    fn a_peer_clock_that_runs_fast_only_loses_the_check() {
        let (base, _) = cuts_over(0.2, 8, |k| (slow_return_path(k), None));
        let (fast, _) = cuts_over(0.2, 8, |k| (slow_return_path(k), Some(0.4 * k as f64)));
        assert!(
            fast >= base * 2 / 3,
            "{fast} cuts with a fast peer clock, {base} without stamps"
        );
        let (slow, _) = cuts_over(0.2, 8, |k| (slow_return_path(k), Some(0.0)));
        assert_eq!(slow, 0);
    }

    /// A saturating sender through a drop-tail bottleneck of `rate` bytes per second with
    /// `buffer_ms` of buffer, in 50 ms rounds on a 60 ms path; `foreign_ms(now)` is queue that
    /// other traffic keeps standing there whatever this sender does. The queue of the sender's
    /// own bytes (milliseconds) and its target at the end of every round.
    fn standing_queue(
        c: &mut RateController,
        rate: f64,
        buffer_ms: f64,
        ms: u64,
        foreign_ms: impl Fn(u64) -> f64,
    ) -> Vec<(f64, u32)> {
        const ROUND_MS: u64 = 50;
        let dt = ROUND_MS as f64 / 1_000.0;
        let (mut queue, mut now) = (0.0f64, 0u64);
        let mut out = Vec::new();
        while now < ms {
            let offered = f64::from(c.target_bytes_per_second()) * dt;
            let before_ms = 1_000.0 * queue / rate;
            let served = (queue + offered).min(rate * dt);
            queue += offered - served;
            let dropped = (queue - buffer_ms / 1_000.0 * rate).max(0.0);
            queue -= dropped;
            let after_ms = 1_000.0 * queue / rate;
            let packets = (offered / 1_200.0).ceil().max(1.0) as u32;
            let lost = ((dropped / 1_200.0).round() as u32).min(packets);
            now += ROUND_MS;
            let foreign = foreign_ms(now);
            c.on_round(
                now,
                &RoundSample {
                    interval_ms: ROUND_MS,
                    acked_bytes: served as u64,
                    sent_bytes: offered as u64,
                    acked_packets: packets - lost,
                    lost_packets: lost,
                    round_min_rtt_ms: 60.0 + before_ms.min(after_ms) + foreign,
                    srtt_ms: 60.0 + after_ms + foreign,
                    min_rtt_ms: 60.0,
                    delay_samples: 0,
                    round_min_one_way_ms: 0.0,
                    min_one_way_ms: 0.0,
                    gradient: None,
                    held_bytes: 1,
                    capped: false,
                    essential_bytes: 0,
                },
            );
            out.push((after_ms, c.target_bytes_per_second()));
        }
        out
    }

    /// ADR-10145: a queue the startup left standing is drained, not held. A sender that starts
    /// at 256 KiB/s into a 32 kB/s link with half a second of buffer (the soak's `congested`
    /// downlink) fills the buffer before the first signal returns. Before, the cut took the
    /// target to 0.85 of the link's rate and held it there, so the 512 ms queue emptied at 15 %
    /// of the link's rate: every packet of the next seconds stood behind it. Now the cut drains
    /// it: over 150 ms of queue for under a second in all, and the rate comes back as soon as the
    /// queue is gone.
    #[test]
    fn a_deep_standing_queue_is_drained_within_a_second() {
        let mut c = RateController::new(RateConfig::default());
        let rounds = standing_queue(&mut c, 32_000.0, 512.0, 6_000, |_| 0.0);
        let deep: Vec<usize> = rounds
            .iter()
            .enumerate()
            .filter(|(_, r)| r.0 > 150.0)
            .map(|(i, _)| i)
            .collect();
        let deep_ms = deep.len() * 50;
        let peak = rounds.iter().map(|r| r.0).fold(0.0, f64::max);
        println!(
            "queue over 150 ms for {deep_ms} ms (rounds {:?}..={:?}), peak {peak:.0} ms; queue ms by round: {:.0?}",
            deep.first(),
            deep.last(),
            rounds.iter().map(|r| r.0).collect::<Vec<_>>()
        );
        println!(
            "target by round: {:?}",
            rounds.iter().map(|r| r.1).collect::<Vec<_>>()
        );
        assert!(
            peak > 400.0,
            "the model's startup fills the buffer: peak {peak:.0} ms"
        );
        assert!(
            deep_ms <= 1_000,
            "the queue stood over 150 ms for {deep_ms} ms of the first 6 s"
        );
        // The rate comes back: from 2 s on the sender offers at least 80 % of the link's rate
        // and never builds 150 ms of queue again.
        let late = &rounds[40..];
        let mean = late.iter().map(|r| f64::from(r.1)).sum::<f64>() / late.len() as f64;
        assert!(mean >= 0.8 * 32_000.0, "mean target {mean:.0} B/s from 2 s on");
        assert!(
            late.iter().all(|r| r.0 <= 150.0),
            "a deep queue again after the drain"
        );
    }

    /// ADR-10145: the drain's shape. A cut that finds a deep queue answers with a rate far under
    /// the bottleneck's for the time the queue needs, a shallow one keeps the plain cut, and the
    /// drain ends in the round whose packets met no queue.
    #[test]
    fn a_drain_is_scaled_to_the_queue_and_ends_when_it_is_gone() {
        let deep = RateController::drain_of(1_000, 512.0, 32_000.0, 60.0);
        assert_eq!(deep.rate, DRAIN_GAIN_MIN * 32_000.0);
        assert_eq!(deep.until_ms, 1_000 + (512.0f64 / 0.75 + 60.0).ceil() as u64);
        let shallow = RateController::drain_of(1_000, 40.0, 32_000.0, 60.0);
        assert_eq!(
            shallow.rate,
            DECREASE_FACTOR * 32_000.0,
            "a drain is never shallower than the plain cut"
        );
        let middle = RateController::drain_of(1_000, 200.0, 32_000.0, 60.0);
        assert_eq!(middle.rate, 0.5 * 32_000.0);
        assert_eq!(middle.until_ms, 1_000 + 460);
        let long = RateController::drain_of(0, 4_000.0, 32_000.0, 60.0);
        assert_eq!(long.until_ms, DRAIN_MAX_MS);

        let mut c = RateController::new(RateConfig::default());
        let rounds = standing_queue(&mut c, 32_000.0, 512.0, 6_000, |_| 0.0);
        assert!(c.drains() >= 1, "the startup's queue was drained");
        assert!(!c.draining(), "no drain is left in force at 6 s");
        // While the queue was deep the sender was held far under the link's rate.
        let held = rounds
            .iter()
            .filter(|r| r.0 > 150.0)
            .map(|r| r.1)
            .min()
            .unwrap_or(u32::MAX);
        assert!(
            f64::from(held) <= 0.5 * 32_000.0,
            "the lowest target with a deep queue standing was {held} B/s"
        );
    }

    /// ADR-10145: a queue that is not the sender's is left alone. From 3 s on cross traffic builds
    /// 300 ms of queue on the path within four rounds and keeps it there whatever the sender
    /// does. The cut drains once, the queue does not move, and the sender goes back to its
    /// ordinary rate under the link instead of starving itself: the drain is refused for the
    /// retry time, and the sender is held under half the link's rate for no longer than two
    /// drains last.
    #[test]
    fn a_queue_the_drain_does_not_move_is_left_alone() {
        let mut c = RateController::new(blind(RateConfig::default()));
        let foreign = |now: u64| (now.saturating_sub(3_000) as f64 * 1.5).min(300.0);
        let ms = 3_000 + DRAIN_RETRY_MS - 1_000;
        let rounds = standing_queue(&mut c, 32_000.0, 512.0, ms, foreign);
        println!(
            "drains {}, refused until {:?}, target by round from 3 s: {:?}",
            c.drains(),
            c.drain_refused_until,
            rounds[60..].iter().map(|r| r.1).collect::<Vec<_>>()
        );
        let low_rounds = rounds[60..]
            .iter()
            .filter(|r| f64::from(r.1) < 0.5 * 32_000.0)
            .count();
        assert!(
            low_rounds * 50 <= 2 * DRAIN_MAX_MS as usize,
            "held under half the link's rate for {} ms by a queue that is not its own",
            low_rounds * 50
        );
        assert!(!c.draining(), "no drain in force at the end");
        assert!(
            c.drain_refused_until.is_some_and(|t| t > 3_000),
            "the drain that did not move the queue is refused for a while: {:?}",
            c.drain_refused_until
        );
    }

    /// One 150 ms round of a first flight: `sent` and `acked` bytes, `held` when the pacer held
    /// data back.
    fn flight_round(sent: u64, acked: u64, held: bool) -> RoundSample {
        RoundSample {
            interval_ms: 150,
            acked_bytes: acked,
            sent_bytes: sent,
            acked_packets: (acked / 1_200).max(1) as u32,
            round_min_rtt_ms: 100.0,
            srtt_ms: 100.0,
            min_rtt_ms: 100.0,
            held_bytes: u64::from(held),
            ..RoundSample::default()
        }
    }

    /// ADR-10200: the round after a first flight acknowledges packets that left at the floor's
    /// rate. Its delivery rate is no evidence against the path, and random loss in it is no loss
    /// cut to that rate: the target stays at the start rate the flight kept. Before, the flight's
    /// filled round counted as the "filled round before" of ADR-10157, and a lossy fat path's
    /// rejoin was cut to about what its flight carried.
    #[test]
    fn the_round_after_a_first_flight_is_no_evidence_against_the_path() {
        let mut c = RateController::new(RateConfig {
            first_flight: true,
            ..RateConfig::default()
        });
        assert!(c.first_flight());
        // The flight's round: filled (held), and the path kept up with it.
        c.on_round(150, &flight_round(14_700, 14_700, true));
        assert!(!c.first_flight());
        let start = c.target_bytes_per_second();
        assert_eq!(start, 262_144, "the path kept up: the start rate stands");
        // The next round fills the start rate; what came back left at the flight's rate
        // (40 kB/s), and a sixth of it was lost at random.
        let next = RoundSample {
            interval_ms: 100,
            sent_bytes: 26_214,
            acked_bytes: 4_000,
            acked_packets: 40,
            lost_packets: 8,
            round_min_rtt_ms: 100.0,
            srtt_ms: 100.0,
            min_rtt_ms: 100.0,
            ..RoundSample::default()
        };
        c.on_round(250, &next);
        let after = c.target_bytes_per_second();
        assert!(
            f64::from(after) >= 0.9 * f64::from(start),
            "the round after the flight cut the target to {after} B/s (start {start})"
        );
    }

    /// ADR-10156: the first flight. With `first_flight` on, a sender is held to the floor's rate
    /// until its first feedback round ends, whatever the start rate says: 256 KiB/s into a path
    /// nothing is known about put a whole join in a thin link's buffer before the first
    /// acknowledgment (the soak's `congested`: 28.8 kB in the first round into 16 KiB of buffer).
    /// A first round the sender filled, on a path that fell behind the flight, then starts the
    /// startup at what the path delivered in it; a path that kept up, and a first round the
    /// sender did not fill, say nothing, so the start rate stands. Off (the default, ADR-10231),
    /// nothing changes: the target is the start rate from the first packet.
    #[test]
    fn the_first_flight_is_held_to_the_floor_and_starts_from_what_it_delivered() {
        let on = RateConfig {
            first_flight: true,
            ..RateConfig::default()
        };
        assert_eq!(on.first_flight_bytes_per_second(), 32 * 1024);
        assert_eq!(on.start_burst_bytes(1_200), 9_830, "300 ms of the floor");
        // Off by default (the lead's revised ruling, ADR-10231): the start rate at once, and
        // ADR-10145's first burst.
        assert!(!RateConfig::default().first_flight && !RateConfig::client().first_flight);
        let off = RateController::new(RateConfig::default());
        assert!(!off.first_flight());
        assert_eq!(off.target_bytes_per_second(), 262_144);
        assert_eq!(RateConfig::default().start_burst_bytes(1_200), usize::MAX);

        // A thin link: the round delivered 4.8 kB in 150 ms (32 kB/s) of the 14.7 kB sent. Of
        // those, 5.1 kB left in the last round trip (100 ms and the 25 ms threshold at 1.25 times
        // the floor); 9.6 kB were due and half of them came back: the path fell behind.
        let mut thin = RateController::new(on.clone());
        assert!(thin.first_flight());
        assert_eq!(thin.target_bytes_per_second(), 32 * 1024, "held to the floor");
        thin.on_round(150, &flight_round(14_700, 4_800, true));
        assert!(!thin.first_flight(), "the path has answered");
        assert_eq!(
            thin.target_bytes_per_second(),
            32 * 1024,
            "a thin link's startup begins at its delivery rate, not under the first flight's"
        );
        // A link a little faster than the floor: 6 kB in 150 ms is 40 kB/s.
        let mut slow = RateController::new(on.clone());
        slow.on_round(150, &flight_round(14_700, 6_000, true));
        assert_eq!(slow.target_bytes_per_second(), 40_000);
        assert_eq!(slow.state(), RateState::Startup, "the startup goes on from there");
        // A path that kept up with the flight (all of it acknowledged, or all but a random 10 %)
        // has shown no limit: the start rate, and not twice it for a round sent at the floor.
        for acked in [14_700, 13_200] {
            let mut fast = RateController::new(on.clone());
            fast.on_round(150, &flight_round(14_700, acked, true));
            assert!(!fast.first_flight());
            assert_eq!(
                fast.target_bytes_per_second(),
                262_144,
                "{acked} bytes acknowledged"
            );
            assert_eq!(fast.state(), RateState::Startup);
        }
        // Never over the start rate, behind or not: a project that set 40 KiB/s starts there.
        let low_start = RateConfig {
            start_bytes_per_second: 40 * 1024,
            ..on.clone()
        };
        let mut capped = RateController::new(low_start.clone());
        assert_eq!(capped.target_bytes_per_second(), 32 * 1024);
        capped.on_round(150, &flight_round(14_700, 14_700, true));
        assert_eq!(capped.target_bytes_per_second(), 40 * 1024);
        let mut capped = RateController::new(low_start);
        capped.on_round(150, &flight_round(14_700, 6_300, true));
        assert_eq!(capped.target_bytes_per_second(), 40 * 1024, "42 kB/s delivered");
        // A round that acknowledged nothing is no answer: the flight goes on.
        let mut silent = RateController::new(on.clone());
        silent.on_round(
            150,
            &RoundSample {
                interval_ms: 150,
                sent_bytes: 14_700,
                held_bytes: 1,
                ..RoundSample::default()
            },
        );
        assert!(silent.first_flight(), "nothing was acknowledged");
        assert_eq!(silent.target_bytes_per_second(), 32 * 1024);
        silent.on_round(300, &flight_round(14_700, 14_700, true));
        assert!(!silent.first_flight());
        assert_eq!(silent.target_bytes_per_second(), 262_144);
        // A first round the sender did not fill says nothing about the path.
        let mut idle = RateController::new(on.clone());
        idle.on_round(150, &flight_round(900, 900, false));
        assert!(!idle.first_flight());
        assert_eq!(idle.target_bytes_per_second(), 262_144);
        // The client's floor is 16 KiB/s; a start rate under the floor is the first flight's.
        let client = RateConfig {
            first_flight: true,
            ..RateConfig::client()
        };
        assert_eq!(client.first_flight_bytes_per_second(), 16 * 1024);
        let low = RateConfig {
            first_flight: true,
            start_bytes_per_second: 8 * 1024,
            ..RateConfig::default()
        };
        assert_eq!(low.first_flight_bytes_per_second(), 8 * 1024);
        assert_eq!(low.start_burst_bytes(1_200), PACING_BURST_PACKETS * 1_200);
    }
}
