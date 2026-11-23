// SPDX-License-Identifier: MPL-2.0
//! Connection quality: one [`PeerQuality`] per link with a [`LinkGrade`] (spec 120 § 13.1,
//! ADR-8182).
//!
//! Both ends keep a `QualityTracker` per link. Every [`QUALITY_SAMPLE_MS`] it samples the
//! connection's cumulative counters into windows of [`QUALITY_WINDOW_MS`] and derives:
//!
//! * round-trip time: the smoothed value (RFC 6298 `SRTT`), its variation (`RTTVAR`) and the p50
//!   and p95 of the raw samples in the window;
//! * one-way jitter: the RFC 3550 § 6.4.1 interarrival jitter of the peer's clock stamps (the
//!   server's tick syncs, the client's clock records), next to `RTTVAR`;
//! * loss in both directions: `loss_out` from this end's acknowledgments, `loss_in` as the peer
//!   measured what this end sent (the server's `LinkFeedback` for a client), and `loss_recv` from
//!   the packet numbers this end received;
//! * input lateness: missing and late input fractions over the window (server-measured; a client
//!   learns them from `LinkFeedback`).
//!
//! The grade is [`GOOD_LIMITS`] / [`FAIR_LIMITS`] on those, with hysteresis: a worse grade shows
//! after it held for [`DEGRADE_HOLD_MS`], a better one after it held for [`RECOVER_HOLD_MS`] with
//! every signal under [`RECOVER_MARGIN`] of the better grade's limit, and `Interrupted` shows at
//! once (it is already debounced by the silence policy's `interrupt_ms`). The degrade timer keeps
//! running while the signals stay past [`RECOVER_MARGIN`] of the limit (a Schmitt band), and a
//! link that received nothing since the last sample moves no grade.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::connection::Connection;

/// The window every quality signal covers, milliseconds (spec 120 § 13.1: "last 10 s").
pub const QUALITY_WINDOW_MS: u64 = 10_000;
/// How often a tracker samples its connection and re-grades, milliseconds.
pub const QUALITY_SAMPLE_MS: u64 = 250;
/// How long a worse grade must hold before it shows, milliseconds. The signals are 10 s window
/// values, so they move slowly; the hold and the [`RECOVER_MARGIN`] band keep the grade steady.
pub const DEGRADE_HOLD_MS: u64 = 500;
/// How long a better grade must hold before it shows, milliseconds (spec 120 § 13.1: 2 s).
pub const RECOVER_HOLD_MS: u64 = 2_000;
/// A better grade counts only while every signal is under this fraction of its limits, so a
/// signal that sits on a limit does not flip the grade back and forth.
pub const RECOVER_MARGIN: f32 = 0.75;
/// Resolved packets a loss window needs before its fraction counts toward the grade: the fewest
/// at which one lost packet stays under [`RECOVER_MARGIN`] of the `Good` loss limit
/// (1 / 67 < 0.75 x 2 %). With fewer, a single loss reads as a loss rate past the limit and a
/// clean link shows `Fair` for seconds after it starts (ADR-8889; it was 32).
pub const MIN_LOSS_PACKETS: u64 = 67;
/// Simulated ticks an input window needs before its fractions count toward the grade.
pub const MIN_INPUT_TICKS: u64 = 30;
/// After an interruption ends, the loss, RTT and input windows restart this long after the
/// restore: packets sent into the outage resolve as lost only once traffic flows again, and they
/// and the ticks simulated without the peer describe the outage, not the link.
pub const RESTORE_SETTLE_MS: u64 = 1_000;
/// RTT samples a tracker keeps at most (a peer cannot grow it).
const MAX_RTT_SAMPLES: usize = 1_024;

/// The limits a link must stay under for a grade (spec 120 § 13.1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GradeLimits {
    /// Round-trip time p95 over the window, milliseconds.
    pub rtt_p95_ms: f32,
    /// Loss fraction, the worst of `loss_out`, `loss_in` and `loss_recv`.
    pub loss: f32,
    /// Fraction of simulated ticks without this peer's input.
    pub inputs_missing: f32,
}

/// `Good`: RTT p95 under 150 ms, loss under 2 %, missing inputs under 0.5 %.
pub const GOOD_LIMITS: GradeLimits = GradeLimits {
    rtt_p95_ms: 150.0,
    loss: 0.02,
    inputs_missing: 0.005,
};
/// `Fair`: RTT p95 under 300 ms, loss under 5 %, missing inputs under 2 %; `Poor` otherwise.
pub const FAIR_LIMITS: GradeLimits = GradeLimits {
    rtt_p95_ms: 300.0,
    loss: 0.05,
    inputs_missing: 0.02,
};

/// How a link feels, for a HUD's connection icon (spec 120 § 13.1). Ordered from best to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkGrade {
    /// Under every [`GOOD_LIMITS`] limit.
    #[default]
    Good,
    /// Under every [`FAIR_LIMITS`] limit.
    Fair,
    /// Over a [`FAIR_LIMITS`] limit.
    Poor,
    /// The peer is silent past the silence policy's `interrupt_ms` (spec 120 § 10.2).
    Interrupted,
}

impl LinkGrade {
    /// `"good"`, `"fair"`, `"poor"` or `"interrupted"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Fair => "fair",
            Self::Poor => "poor",
            Self::Interrupted => "interrupted",
        }
    }
}

impl std::fmt::Display for LinkGrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the transport knows about one peer's link, for the game's HUD and its own decisions
/// (spec 120 § 13.1). Fractions are `0.0..=1.0`. Signals without enough samples read `0`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerQuality {
    /// Smoothed round-trip time (RFC 6298 `SRTT`), milliseconds.
    pub rtt_ms: f32,
    /// Median raw round-trip sample over the last [`QUALITY_WINDOW_MS`], milliseconds.
    pub rtt_p50_ms: f32,
    /// 95th percentile raw round-trip sample over the same window, milliseconds.
    pub rtt_p95_ms: f32,
    /// Round-trip variation (RFC 6298 `RTTVAR`), milliseconds.
    pub rttvar_ms: f32,
    /// One-way interarrival jitter of what the peer sends (RFC 3550 § 6.4.1), milliseconds.
    pub jitter_ms: f32,
    /// Loss on what this end sent, as this end measured it from acknowledgments, last 10 s.
    pub loss_out: f32,
    /// Loss on what this end sent, as the peer measured it: for a client the upstream loss the
    /// server reports in `LinkFeedback`. `0` on the server (clients send no loss report).
    pub loss_in: f32,
    /// Loss on what the peer sent this end, measured here from packet numbers, last 10 s: for a
    /// client the downstream loss; for the server the upstream loss it reports to the client.
    pub loss_recv: f32,
    /// Queueing delay over the minimum RTT in the rate controller's latest round, milliseconds
    /// (spec 120 § 4.2; `LinkStats::queue_delay_ms`, ADR-8889).
    pub queue_delay_ms: f32,
    /// Rate the controller lets the game's own traffic use now, bytes per second
    /// (`LinkStats::send_allowance_bps`; `u32::MAX` with the controller off, ADR-8889).
    pub send_allowance_bps: u32,
    /// What this end sent over the last second, bytes per second.
    pub sent_bps: u32,
    /// The server-measured 5th percentile input arrival margin, ticks (`LinkFeedback`).
    pub input_margin_p5: i16,
    /// Fraction of simulated ticks whose input arrived after the tick, last 10 s.
    pub inputs_late: f32,
    /// Fraction of simulated ticks without this peer's input, last 10 s.
    pub inputs_missing: f32,
    /// The grade, with hysteresis.
    pub grade: LinkGrade,
    /// Whether the link is interrupted now.
    pub interrupted: bool,
}

impl PeerQuality {
    /// The worst of the three loss fractions.
    pub fn loss(&self) -> f32 {
        self.loss_out.max(self.loss_in).max(self.loss_recv)
    }

    /// The grade these signals have now, without hysteresis and without `Interrupted`.
    pub fn instant_grade(&self) -> LinkGrade {
        if self.within(&GOOD_LIMITS, 1.0) {
            LinkGrade::Good
        } else if self.within(&FAIR_LIMITS, 1.0) {
            LinkGrade::Fair
        } else {
            LinkGrade::Poor
        }
    }

    /// Whether every signal is under `margin` times `limits`.
    pub fn within(&self, limits: &GradeLimits, margin: f32) -> bool {
        self.rtt_p95_ms < limits.rtt_p95_ms * margin
            && self.loss() < limits.loss * margin
            && self.inputs_missing < limits.inputs_missing * margin
    }
}

/// A grade change, as `ClientEvent::QualityChanged` and `ServerEvent::QualityChanged` carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GradeChange {
    /// The grade shown before.
    pub from: LinkGrade,
    /// The grade shown now.
    pub to: LinkGrade,
}

/// RFC 3550 § 6.4.1 interarrival jitter: `J += (|D(i-1, i)| - J) / 16`, where `D` is the
/// difference of two consecutive arrivals' transit times (arrival minus the sender's stamp).
#[derive(Debug, Clone, Default)]
pub struct InterarrivalJitter {
    /// The sender stamp of the previous arrival, its unwrapped value and its transit time.
    last: Option<(u32, i64, i64)>,
    jitter: f64,
    samples: u64,
}

impl InterarrivalJitter {
    /// Takes one arrival at `arrival_ms` (this end's clock) of a packet the peer stamped
    /// `sent_ms` (its clock, wrapping).
    pub fn on_arrival(&mut self, sent_ms: u32, arrival_ms: u64) {
        let arrival = i64::try_from(arrival_ms).unwrap_or(i64::MAX);
        let sent = match self.last {
            None => i64::from(sent_ms),
            Some((last_raw, last_sent, _)) => last_sent + i64::from(sent_ms.wrapping_sub(last_raw) as i32),
        };
        let transit = arrival.saturating_sub(sent);
        if let Some((_, _, last_transit)) = self.last {
            let d = transit.saturating_sub(last_transit).unsigned_abs() as f64;
            self.jitter += (d - self.jitter) / 16.0;
            self.samples += 1;
        }
        self.last = Some((sent_ms, sent, transit));
    }

    /// The jitter estimate, milliseconds (`0` before two arrivals).
    pub fn jitter_ms(&self) -> f64 {
        self.jitter
    }

    /// Pairs of arrivals taken so far.
    pub fn samples(&self) -> u64 {
        self.samples
    }
}

/// Cumulative counter pairs sampled over time; the difference across the window.
#[derive(Debug, Clone, Default)]
struct CounterWindow {
    samples: VecDeque<(u64, u64, u64)>,
}

impl CounterWindow {
    fn push(&mut self, now: u64, a: u64, b: u64) {
        self.samples.push_back((now, a, b));
        // Keep the newest sample at or before the window start as the base.
        let start = now.saturating_sub(QUALITY_WINDOW_MS);
        while self.samples.len() > 1 && self.samples.get(1).is_some_and(|(t, _, _)| *t <= start) {
            self.samples.pop_front();
        }
    }

    fn delta(&self) -> (u64, u64) {
        match (self.samples.front(), self.samples.back()) {
            (Some(first), Some(last)) => (last.1.saturating_sub(first.1), last.2.saturating_sub(first.2)),
            _ => (0, 0),
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
    }
}

/// Packet numbers seen from the peer, unwrapped: how many it sent (progress of its newest
/// packet number) against how many arrived.
#[derive(Debug, Clone, Default)]
pub(crate) struct SeqProgress {
    last: Option<u16>,
    spanned: u64,
}

impl SeqProgress {
    /// Whether a first packet number was seen.
    pub(crate) fn has_base(&self) -> bool {
        self.last.is_some()
    }

    /// Takes the newest received packet number; returns the packet numbers spanned so far. A jump
    /// of half the sequence space or more is not progress (a wrap or a reset).
    pub(crate) fn update(&mut self, newest: Option<u16>) -> u64 {
        if let Some(seq) = newest {
            if let Some(last) = self.last {
                let span = seq.wrapping_sub(last);
                if span < 1 << 15 {
                    self.spanned += u64::from(span);
                    self.last = Some(seq);
                }
            } else {
                self.last = Some(seq);
            }
        }
        self.spanned
    }
}

/// Loss fraction of `lost` among `total`, or `0` with fewer than [`MIN_LOSS_PACKETS`].
fn fraction(lost: u64, total: u64) -> f32 {
    if total < MIN_LOSS_PACKETS {
        0.0
    } else {
        (lost.min(total) as f64 / total as f64) as f32
    }
}

/// The input side of one sample: totals since the join.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InputTotals {
    /// The server's next tick to simulate.
    pub(crate) server_tick: u64,
    /// Inputs whose first copy arrived after their tick.
    pub(crate) late: u64,
    /// Ticks simulated without the peer's input.
    pub(crate) missing: u64,
    /// The 5th percentile arrival margin, ticks.
    pub(crate) margin_p5: i32,
}

/// One link's quality tracker. See the module docs.
#[derive(Debug, Default)]
pub(crate) struct QualityTracker {
    rtt: VecDeque<(u64, f32)>,
    scratch: Vec<f32>,
    rtt_count: u64,
    jitter: InterarrivalJitter,
    out: CounterWindow,
    recv: CounterWindow,
    seq: SeqProgress,
    late: CounterWindow,
    missing: CounterWindow,
    next_sample_ms: Option<u64>,
    settle_until: Option<u64>,
    quality: PeerQuality,
    /// The grade the signals point at and since when, while it differs from the shown one.
    pending: Option<(LinkGrade, u64)>,
    /// The grade shown before the current interruption.
    before_interrupt: LinkGrade,
    /// Packets received at the previous sample: a sample with none new is a silent link.
    last_received: u64,
}

impl QualityTracker {
    /// The latest quality.
    pub(crate) fn quality(&self) -> PeerQuality {
        self.quality
    }

    /// Takes one clock stamp of the peer that arrived at `now`.
    pub(crate) fn on_stamp(&mut self, sent_ms: u32, now: u64) {
        self.jitter.on_arrival(sent_ms, now);
    }

    /// Moves the link in or out of the interrupted state at once; returns the change.
    pub(crate) fn set_interrupted(&mut self, interrupted: bool, now: u64) -> Option<GradeChange> {
        if self.quality.interrupted == interrupted {
            return None;
        }
        self.quality.interrupted = interrupted;
        self.pending = None;
        let from = self.quality.grade;
        let to = if interrupted {
            self.before_interrupt = from;
            LinkGrade::Interrupted
        } else {
            self.settle_until = Some(now + RESTORE_SETTLE_MS);
            self.before_interrupt
        };
        self.quality.grade = to;
        (from != to).then_some(GradeChange { from, to })
    }

    /// Samples `conn` at most every [`QUALITY_SAMPLE_MS`] and re-grades; returns a change of the
    /// shown grade. `inputs` are the server's input totals for this peer (none for a spectator
    /// or before the first report); `loss_in` is the peer's report of what this end sent.
    pub(crate) fn sample(
        &mut self,
        now: u64,
        conn: &Connection,
        inputs: Option<InputTotals>,
        loss_in: Option<f32>,
    ) -> Option<GradeChange> {
        // Raw RTT samples are polled every call so few are missed between samples.
        let (count, latest) = conn.rtt_samples();
        if count != self.rtt_count {
            self.rtt_count = count;
            if self.rtt.len() >= MAX_RTT_SAMPLES {
                self.rtt.pop_front();
            }
            self.rtt.push_back((now, latest as f32));
        }
        if self.next_sample_ms.is_some_and(|t| now < t) {
            return None;
        }
        self.next_sample_ms = Some(now + QUALITY_SAMPLE_MS);
        let _span = tracing::trace_span!("regia_net::quality_sample").entered();
        if self.settle_until.is_some_and(|t| now >= t) {
            self.settle_until = None;
            self.out.clear();
            self.recv.clear();
            self.rtt.clear();
            // Ticks simulated without the peer during the outage say nothing about the link.
            self.late.clear();
            self.missing.clear();
        }
        let stats = conn.stats();
        self.out.push(now, stats.packets_acked, stats.packets_lost);
        // Packets received before the first packet number was seen have no span to count against:
        // the window starts at the first sample with a baseline.
        let had_base = self.seq.has_base();
        let spanned = self.seq.update(conn.remote_seq());
        if had_base {
            self.recv.push(now, spanned, stats.packets_received);
        } else {
            self.recv.clear();
        }
        let start = now.saturating_sub(QUALITY_WINDOW_MS);
        while self.rtt.front().is_some_and(|(t, _)| *t < start) {
            self.rtt.pop_front();
        }
        self.scratch.clear();
        self.scratch.extend(self.rtt.iter().map(|(_, r)| *r));
        self.scratch.sort_unstable_by(f32::total_cmp);
        let pct = |s: &[f32], p: usize| s.get((s.len().max(1) - 1) * p / 100).copied().unwrap_or(0.0);
        let q = &mut self.quality;
        q.rtt_ms = stats.rtt_ms as f32;
        q.rttvar_ms = stats.jitter_ms as f32;
        q.rtt_p50_ms = pct(&self.scratch, 50);
        q.rtt_p95_ms = pct(&self.scratch, 95);
        q.jitter_ms = self.jitter.jitter_ms() as f32;
        let (acked, lost) = self.out.delta();
        q.loss_out = fraction(lost, acked + lost);
        let (spanned, got) = self.recv.delta();
        q.loss_recv = fraction(spanned.saturating_sub(got), spanned);
        q.loss_in = loss_in.unwrap_or(0.0).clamp(0.0, 1.0);
        q.sent_bps = (stats.send_kbps * 125.0).round().clamp(0.0, f64::from(u32::MAX)) as u32;
        // The rate controller's view of the path (spec 120 § 4.2, ADR-8889).
        q.queue_delay_ms = stats.queue_delay_ms as f32;
        q.send_allowance_bps = stats.send_allowance_bps;
        if let Some(i) = inputs {
            self.late.push(now, i.server_tick, i.late);
            self.missing.push(now, i.server_tick, i.missing);
            let (ticks, late) = self.late.delta();
            let (_, missing) = self.missing.delta();
            let frac = |n: u64| {
                if ticks < MIN_INPUT_TICKS {
                    0.0
                } else {
                    (n.min(ticks) as f64 / ticks as f64) as f32
                }
            };
            q.inputs_late = frac(late);
            q.inputs_missing = frac(missing);
            q.input_margin_p5 = i.margin_p5.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        }
        // A silent link is the silence policy's to judge (`Interrupted` after `interrupt_ms`): what
        // the windows show while nothing arrives is the outage, not the link, so no grade moves.
        let silent = stats.packets_received == self.last_received;
        self.last_received = stats.packets_received;
        if q.interrupted || self.settle_until.is_some() || silent {
            self.pending = None;
            return None;
        }
        self.regrade(now)
    }

    fn regrade(&mut self, now: u64) -> Option<GradeChange> {
        let q = &self.quality;
        let shown = q.grade;
        // A Schmitt trigger per grade boundary: `strict` crosses at the limits, `banded` at
        // RECOVER_MARGIN of them, so `strict <= banded`. A degrade starts when `strict` passes the
        // shown grade and keeps its timer while `banded` stays past it; a recovery starts when
        // `banded` is better than the shown grade and keeps its timer while it stays better.
        let strict = q.instant_grade();
        let banded = if q.within(&GOOD_LIMITS, RECOVER_MARGIN) {
            LinkGrade::Good
        } else if q.within(&FAIR_LIMITS, RECOVER_MARGIN) {
            LinkGrade::Fair
        } else {
            LinkGrade::Poor
        };
        let pending = self.pending.filter(|(g, _)| {
            if *g > shown {
                banded > shown
            } else {
                banded < shown
            }
        });
        // A degrade shows the strict grade, or one step worse than the shown one while the signals
        // sit in the band past its limit.
        let one_worse = match shown {
            LinkGrade::Good => LinkGrade::Fair,
            _ => LinkGrade::Poor,
        };
        let (candidate, since) = match pending {
            Some((g, since)) if g > shown => (strict.max(one_worse), since),
            Some((_, since)) => (banded, since),
            None if strict > shown => (strict, now),
            None if banded < shown => (banded, now),
            None => {
                self.pending = None;
                return None;
            }
        };
        self.pending = Some((candidate, since));
        let hold = if candidate > shown {
            DEGRADE_HOLD_MS
        } else {
            RECOVER_HOLD_MS
        };
        if now.saturating_sub(since) < hold {
            return None;
        }
        self.pending = None;
        self.quality.grade = candidate;
        Some(GradeChange {
            from: shown,
            to: candidate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(rtt_p95_ms: f32, loss: f32, missing: f32) -> PeerQuality {
        PeerQuality {
            rtt_p95_ms,
            loss_out: loss,
            inputs_missing: missing,
            ..PeerQuality::default()
        }
    }

    #[test]
    fn grade_thresholds_are_the_spec_rows() {
        assert_eq!(q(149.0, 0.019, 0.004).instant_grade(), LinkGrade::Good);
        assert_eq!(q(150.0, 0.0, 0.0).instant_grade(), LinkGrade::Fair);
        assert_eq!(q(0.0, 0.02, 0.0).instant_grade(), LinkGrade::Fair);
        assert_eq!(q(0.0, 0.0, 0.005).instant_grade(), LinkGrade::Fair);
        assert_eq!(q(299.0, 0.049, 0.019).instant_grade(), LinkGrade::Fair);
        assert_eq!(q(300.0, 0.0, 0.0).instant_grade(), LinkGrade::Poor);
        assert_eq!(q(0.0, 0.05, 0.0).instant_grade(), LinkGrade::Poor);
        assert_eq!(q(0.0, 0.0, 0.02).instant_grade(), LinkGrade::Poor);
        // The worst direction counts.
        let mut two_way = q(0.0, 0.0, 0.0);
        two_way.loss_recv = 0.06;
        assert_eq!(two_way.instant_grade(), LinkGrade::Poor);
        two_way.loss_recv = 0.0;
        two_way.loss_in = 0.03;
        assert_eq!(two_way.instant_grade(), LinkGrade::Fair);
    }

    #[test]
    fn grades_order_best_to_worst_and_name_lowercase() {
        assert!(LinkGrade::Good < LinkGrade::Fair && LinkGrade::Fair < LinkGrade::Poor);
        assert!(LinkGrade::Poor < LinkGrade::Interrupted);
        for g in [
            LinkGrade::Good,
            LinkGrade::Fair,
            LinkGrade::Poor,
            LinkGrade::Interrupted,
        ] {
            // The serde name is the lowercase name the agent protocol shows.
            let text = ron::to_string(&g).expect("ron");
            assert_eq!(text.to_lowercase(), g.as_str(), "{text}");
            assert_eq!(g.to_string(), g.as_str());
        }
    }

    #[test]
    fn interarrival_jitter_follows_rfc_3550() {
        // Constant transit: no jitter.
        let mut j = InterarrivalJitter::default();
        for i in 0..100u32 {
            j.on_arrival(i * 20, u64::from(i * 20) + 50);
        }
        assert_eq!(j.jitter_ms(), 0.0);
        assert_eq!(j.samples(), 99);
        // Transit alternating 50/60 ms: |D| = 10 every time, so J converges to 10.
        let mut j = InterarrivalJitter::default();
        for i in 0..400u32 {
            let transit = if i % 2 == 0 { 50 } else { 60 };
            j.on_arrival(i * 20, u64::from(i * 20 + transit));
        }
        assert!((j.jitter_ms() - 10.0).abs() < 0.01, "{}", j.jitter_ms());
        // One step of 16 ms from zero moves J by exactly 1 ms (gain 1/16).
        let mut j = InterarrivalJitter::default();
        j.on_arrival(0, 100);
        j.on_arrival(20, 136);
        assert!((j.jitter_ms() - 1.0).abs() < 1e-9);
        // The sender clock wraps: transit stays constant across the wrap.
        let mut j = InterarrivalJitter::default();
        for i in 0..10u32 {
            let sent = (u32::MAX - 50).wrapping_add(i * 20);
            j.on_arrival(sent, 1_000 + u64::from(i) * 20);
        }
        assert_eq!(j.jitter_ms(), 0.0);
        // A reordered arrival uses arrival order: transit 50, 70, 50 gives |D| 20 then 20.
        let mut j = InterarrivalJitter::default();
        j.on_arrival(40, 90);
        j.on_arrival(20, 90);
        j.on_arrival(60, 110);
        let expected = 20.0 / 16.0 + (20.0 - 20.0 / 16.0) / 16.0;
        assert!((j.jitter_ms() - expected).abs() < 1e-9);
    }

    #[test]
    fn packet_number_progress_unwraps_and_ignores_resets() {
        let mut s = SeqProgress::default();
        assert_eq!(s.update(None), 0);
        assert_eq!(s.update(Some(65_500)), 0);
        // Across the wrap: 65_500 -> 64 spans 100 numbers.
        assert_eq!(s.update(Some(64)), 100);
        // An older number (reordered) is not progress.
        assert_eq!(s.update(Some(10)), 100);
        // Half the space or more is a reset, not progress.
        assert_eq!(s.update(Some(40_000)), 100);
        assert_eq!(s.update(Some(74)), 110);
    }

    #[test]
    fn counter_windows_cover_ten_seconds() {
        let mut w = CounterWindow::default();
        for t in 0..=80u64 {
            // 4 per sample of 250 ms, 1 of them lost.
            w.push(t * 250, t * 3, t);
        }
        // 20 s pushed; the window spans the last 10 s (40 samples).
        assert_eq!(w.delta(), (120, 40));
        assert!((fraction(40, 160) - 0.25).abs() < 1e-6);
        assert_eq!(fraction(10, MIN_LOSS_PACKETS - 1), 0.0, "too few packets");
    }

    /// ADR-8889: one lost packet among the fewest packets that count is under the `Good` band,
    /// and among one packet fewer it would not be (the minimum is the smallest such count).
    #[test]
    fn one_loss_never_reads_past_the_good_band() {
        let band = GOOD_LIMITS.loss * RECOVER_MARGIN;
        assert!(fraction(1, MIN_LOSS_PACKETS) < band);
        assert!(1.0 / (MIN_LOSS_PACKETS - 1) as f32 >= band);
    }

    /// Drives `regrade` from hand-set signals.
    fn tracker_with(signals: PeerQuality, grade: LinkGrade) -> QualityTracker {
        QualityTracker {
            quality: PeerQuality { grade, ..signals },
            ..QualityTracker::default()
        }
    }

    #[test]
    fn a_worse_grade_shows_after_the_degrade_hold() {
        let mut t = tracker_with(q(0.0, 0.03, 0.0), LinkGrade::Good);
        assert_eq!(t.regrade(0), None);
        assert_eq!(t.regrade(DEGRADE_HOLD_MS - 1), None);
        assert_eq!(
            t.regrade(DEGRADE_HOLD_MS),
            Some(GradeChange {
                from: LinkGrade::Good,
                to: LinkGrade::Fair
            })
        );
        // A spike shorter than the hold never shows: dropping under the band cancels it.
        let h = DEGRADE_HOLD_MS;
        let mut t = tracker_with(q(0.0, 0.03, 0.0), LinkGrade::Good);
        assert_eq!(t.regrade(0), None);
        t.quality.loss_out = 0.0;
        assert_eq!(t.regrade(h / 2), None);
        t.quality.loss_out = 0.03;
        assert_eq!(t.regrade(h / 2 + 10), None);
        assert_eq!(t.regrade(h + 10), None, "the hold restarted at h/2 + 10");
        assert!(t.regrade(h / 2 + 10 + h).is_some());
        // Wobbling on the limit inside the band (1.5-2 %) keeps the degrade's timer.
        let mut t = tracker_with(q(0.0, 0.021, 0.0), LinkGrade::Good);
        assert_eq!(t.regrade(0), None);
        t.quality.loss_out = 0.017;
        assert_eq!(t.regrade(DEGRADE_HOLD_MS / 2), None);
        assert_eq!(t.regrade(DEGRADE_HOLD_MS).map(|c| c.to), Some(LinkGrade::Fair));
        // A degrade shows the strict grade at the end of the hold: a spike to Poor that falls back
        // to Fair shows Fair; one that stays Poor shows Poor.
        let mut t = tracker_with(q(0.0, 0.08, 0.0), LinkGrade::Good);
        assert_eq!(t.regrade(0), None);
        t.quality.loss_out = 0.03;
        assert_eq!(t.regrade(DEGRADE_HOLD_MS).map(|c| c.to), Some(LinkGrade::Fair));
        let mut t = tracker_with(q(0.0, 0.03, 0.0), LinkGrade::Good);
        assert_eq!(t.regrade(0), None);
        t.quality.loss_out = 0.08;
        assert_eq!(t.regrade(DEGRADE_HOLD_MS).map(|c| c.to), Some(LinkGrade::Poor));
    }

    #[test]
    fn a_better_grade_needs_the_margin_and_the_recover_hold() {
        // Loss 1.8 %: under the Good limit but over 75 % of it, so Fair stays.
        let mut t = tracker_with(q(0.0, 0.018, 0.0), LinkGrade::Fair);
        for ms in (0..10_000).step_by(250) {
            assert_eq!(t.regrade(ms), None);
        }
        // 1 %: Good after two seconds.
        t.quality.loss_out = 0.01;
        assert_eq!(t.regrade(10_000), None);
        assert_eq!(t.regrade(10_000 + RECOVER_HOLD_MS - 1), None);
        assert_eq!(
            t.regrade(10_000 + RECOVER_HOLD_MS),
            Some(GradeChange {
                from: LinkGrade::Fair,
                to: LinkGrade::Good
            })
        );
        // Poor to Fair when only Fair's margin is met.
        let mut t = tracker_with(q(200.0, 0.0, 0.0), LinkGrade::Poor);
        assert_eq!(t.regrade(0), None);
        assert_eq!(t.regrade(RECOVER_HOLD_MS).map(|c| c.to), Some(LinkGrade::Fair));
    }

    #[test]
    fn interruption_shows_at_once_and_restores_the_grade_before_it() {
        let mut t = tracker_with(q(0.0, 0.03, 0.0), LinkGrade::Fair);
        assert_eq!(
            t.set_interrupted(true, 5),
            Some(GradeChange {
                from: LinkGrade::Fair,
                to: LinkGrade::Interrupted
            })
        );
        assert_eq!(t.set_interrupted(true, 6), None, "once per outage");
        assert_eq!(
            t.set_interrupted(false, 3_000),
            Some(GradeChange {
                from: LinkGrade::Interrupted,
                to: LinkGrade::Fair
            })
        );
        assert_eq!(t.quality().grade, LinkGrade::Fair);
        assert_eq!(t.settle_until, Some(3_000 + RESTORE_SETTLE_MS));
    }
}
