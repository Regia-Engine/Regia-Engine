// SPDX-License-Identifier: MPL-2.0
//! Input timing feedback (spec 120 § 6.4, § 9.1, § 9.2; ADR-8077).
//!
//! A client cannot tell from its own measurements that its inputs reach the server late: when only
//! the downstream path queues, every tick sync arrives late, the client's tick estimate lags, and
//! the round-trip time hides on which leg the delay sits. The server can tell, so it reports.
//!
//! * The server keeps one `InputArrivals` record per player and sends a [`LinkFeedback`] on the
//!   essential unreliable class with every tick sync and at once (at most once per tick) after a
//!   late or missing input: the newest contiguous input tick, the input arrival margin p50 and p5
//!   over the last [`FEEDBACK_WINDOW_MS`], late, missing and too-far-ahead counts, and the upstream
//!   loss its quality tracker measured (`crate::quality`, ADR-8182).
//! * The client keeps its estimate of the server tick with an NTP-style clock filter
//!   (`ClockFilter`, RFC 5905 § 10): of the last [`CLOCK_FILTER_SAMPLES`] tick syncs the one with
//!   the smallest delay sets the offset, and the estimate is never moved backward; it slews by up
//!   to [`CLOCK_SLEW`] and steps forward only over [`CLOCK_STEP_TICKS`] ticks (a counted resync).
//! * The client steers its input lead from the reported margin (`LeadController`): the target is
//!   a p5 margin of at least [`TARGET_MARGIN_TICKS`]; a deficit raises the lead at once, a calm
//!   [`LEAD_CALM_MS`] lowers it by one, and the lead stays inside the server's input buffer.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::wire::{WireError, WireReader, WireWriter};

/// The window the server's arrival margin percentiles cover, milliseconds.
pub const FEEDBACK_WINDOW_MS: u64 = 2_000;
/// The p5 input arrival margin the client's lead aims for, in ticks: one tick of spare time.
pub const TARGET_MARGIN_TICKS: i32 = 1;
/// How long the margin must stay at least one tick above the target before the lead drops by one
/// tick, milliseconds.
pub const LEAD_CALM_MS: u64 = 5_000;
/// Tick syncs the client's clock filter keeps; the one with the smallest delay sets the offset.
pub const CLOCK_FILTER_SAMPLES: usize = 8;
/// The fastest the client's tick estimate runs faster or slower than its own clock while it meets
/// a new offset: 5 %.
pub const CLOCK_SLEW: f64 = 0.05;
/// A tick estimate further behind its target than this many ticks steps forward at once (a
/// resync, counted) instead of slewing.
pub const CLOCK_STEP_TICKS: u64 = 8;
/// Encoded size of a [`LinkFeedback`] after its lane byte, bytes.
pub const LINK_FEEDBACK_BYTES: usize = 8 + 8 + 2 + 2 + 8 + 2 + 4 + 4 + 4 + 2 + 2;

/// Arrival samples one peer's window holds at most (a hostile client cannot grow it).
const MAX_WINDOW_SAMPLES: usize = 512;
/// Input ticks behind the newest one whose arrival the server still tells apart.
const SEEN_TICKS: u64 = 64;
/// Feedback intervals the upstream loss of [`LossMeter`] covers.
const LOSS_INTERVALS: usize = 8;
/// Stamped ticks the client remembers for the lead correction.
const MAX_STAMPS: usize = 512;

/// What the server tells a client about its inputs (spec 120 § 6.4). Sent on the essential
/// unreliable class with every tick sync and right after a late or missing input; counters are
/// totals since the peer joined, so a lost record loses nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LinkFeedback {
    /// The server's next tick to simulate when it wrote this record.
    pub server_tick: u64,
    /// Newest input tick up to which every input arrived or no longer matters (the tick was
    /// already simulated); `None` before the first input.
    pub newest_contiguous_tick: Option<u64>,
    /// Median arrival margin over the last [`FEEDBACK_WINDOW_MS`], ticks: the input's tick minus
    /// the server's next tick when its first copy arrived. Negative: it arrived after its tick.
    pub margin_p50: i32,
    /// 5th percentile arrival margin over the same window, ticks.
    pub margin_p5: i32,
    /// The input tick of the arrival that set `margin_p5`, so the client can tell how much further
    /// ahead it stamps now than it did for that input; `None` without samples.
    pub margin_p5_tick: Option<u64>,
    /// Arrivals the margins were computed from; `0` means the margins carry no information.
    pub margin_samples: u32,
    /// Inputs whose first copy arrived after their tick was simulated.
    pub inputs_late: u64,
    /// Ticks simulated without this peer's input.
    pub inputs_missing: u64,
    /// Inputs dropped for being at or past the server's input buffer (adversarial row 3).
    pub inputs_too_far_ahead: u64,
    /// Fraction of the client's packets the server never received, over the last 10 s (the
    /// server's `PeerQuality::loss_recv`, ADR-8182).
    pub upstream_loss: f32,
    /// The server's input buffer, ticks: an input at or past `server tick + this` is dropped.
    pub input_buffer_ticks: u32,
}

impl LinkFeedback {
    /// Writes the record (without its lane byte); counters saturate at `u32::MAX` and margins at
    /// the `i16` range on the wire.
    pub fn encode(&self, w: &mut WireWriter) {
        w.u64(self.server_tick);
        w.u64(self.newest_contiguous_tick.unwrap_or(u64::MAX));
        w.u16(clamp_i16(self.margin_p50) as u16);
        w.u16(clamp_i16(self.margin_p5) as u16);
        w.u64(self.margin_p5_tick.unwrap_or(u64::MAX));
        w.u16(u16::try_from(self.margin_samples).unwrap_or(u16::MAX));
        w.u32(u32::try_from(self.inputs_late).unwrap_or(u32::MAX));
        w.u32(u32::try_from(self.inputs_missing).unwrap_or(u32::MAX));
        w.u32(u32::try_from(self.inputs_too_far_ahead).unwrap_or(u32::MAX));
        w.u16((self.upstream_loss.clamp(0.0, 1.0) * 10_000.0).round() as u16);
        w.u16(u16::try_from(self.input_buffer_ticks).unwrap_or(u16::MAX));
    }

    /// Reads a record written by [`LinkFeedback::encode`]. A loss over 100 % or an empty input
    /// buffer is malformed.
    pub fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let server_tick = r.u64()?;
        let newest = r.u64()?;
        let margin_p50 = i32::from(r.u16()? as i16);
        let margin_p5 = i32::from(r.u16()? as i16);
        let p5_tick = r.u64()?;
        let margin_samples = u32::from(r.u16()?);
        let inputs_late = u64::from(r.u32()?);
        let inputs_missing = u64::from(r.u32()?);
        let inputs_too_far_ahead = u64::from(r.u32()?);
        let loss = r.u16()?;
        let input_buffer_ticks = u32::from(r.u16()?);
        if loss > 10_000 {
            return Err(WireError::Invalid("feedback loss over 100 %"));
        }
        if input_buffer_ticks == 0 {
            return Err(WireError::Invalid("feedback input buffer of zero ticks"));
        }
        Ok(Self {
            server_tick,
            newest_contiguous_tick: (newest != u64::MAX).then_some(newest),
            margin_p50,
            margin_p5,
            margin_p5_tick: (p5_tick != u64::MAX).then_some(p5_tick),
            margin_samples,
            inputs_late,
            inputs_missing,
            inputs_too_far_ahead,
            upstream_loss: f32::from(loss) / 10_000.0,
            input_buffer_ticks,
        })
    }
}

/// A client's input timing, as `net.status` reports it (ADR-8077).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct InputTiming {
    /// Ticks ahead of the estimated server tick that inputs are stamped
    /// (`NetClient::input_lead_ticks`).
    pub lead_ticks: u32,
    /// Forward steps of the tick estimate over [`CLOCK_STEP_TICKS`] ticks.
    pub clock_resyncs: u64,
    /// Times the server reported the inputs late (`ClientEvent::InputsLate`).
    pub late_spells: u64,
    /// The server's last input feedback; `None` before the first.
    pub feedback: Option<LinkFeedback>,
    /// Inputs the client still repeats: sent and not yet acknowledged by the server's newest
    /// contiguous input tick (spec 120 § 6.1 unacked-only redundancy, ADR-8312).
    #[serde(default)]
    pub unacked_inputs: u32,
    /// Datagrams each input rides in at least, from the loss the client sees
    /// (`tick::loss_aware_copies`, spec 120 § 6.1 item 3); 0 before the first input.
    #[serde(default)]
    pub input_copies: u8,
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// The server's view of one player's input arrivals (spec 120 § 6.4).
#[derive(Debug, Default)]
pub(crate) struct InputArrivals {
    /// Newest input tick that arrived.
    newest: Option<u64>,
    /// Bit `i`: the input of tick `newest - i` arrived.
    seen: u64,
    contiguous: Option<u64>,
    /// `(arrival ms, margin, input tick)` of first copies, oldest first.
    window: VecDeque<(u64, i32, u64)>,
    scratch: Vec<(i32, u64)>,
    late: u64,
    missing: u64,
    last_missed_total: u64,
    /// A late or missing input since the last record.
    urgent: bool,
    /// The tick the last record went out on.
    last_sent_tick: Option<u64>,
}

impl InputArrivals {
    /// Whether this peer has sent an input yet (spectators and players in a menu get no records).
    pub(crate) fn active(&self) -> bool {
        self.newest.is_some()
    }

    /// Inputs whose first copy arrived after their tick was simulated, so far.
    pub(crate) fn late_total(&self) -> u64 {
        self.late
    }

    fn is_seen(&self, tick: u64) -> bool {
        self.newest
            .is_some_and(|n| tick <= n && n - tick < SEEN_TICKS && (self.seen >> (n - tick)) & 1 == 1)
    }

    /// Records one input copy for `tick` arriving at `now_ms` while the server's next tick is
    /// `server_tick`. Only the first copy of a tick counts; returns whether this was it.
    pub(crate) fn record(&mut self, tick: u64, server_tick: u64, now_ms: u64) -> bool {
        match self.newest {
            None => {
                self.newest = Some(tick);
                self.seen = 1;
            }
            Some(n) if tick > n => {
                let shift = tick - n;
                self.seen = if shift >= SEEN_TICKS {
                    0
                } else {
                    self.seen << shift
                } | 1;
                self.newest = Some(tick);
            }
            Some(n) => {
                let behind = n - tick;
                if behind >= SEEN_TICKS || (self.seen >> behind) & 1 == 1 {
                    return false;
                }
                self.seen |= 1 << behind;
            }
        }
        let margin = (i128::from(tick) - i128::from(server_tick))
            .clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32;
        if margin < 0 {
            self.late += 1;
            self.urgent = true;
        }
        if self.window.len() >= MAX_WINDOW_SAMPLES {
            self.window.pop_front();
        }
        self.window.push_back((now_ms, margin, tick));
        true
    }

    /// The median arrival margin over the last [`FEEDBACK_WINDOW_MS`] before `now_ms`, ticks, and
    /// the arrivals it was taken from (`(0, 0)` without any). Builds no record and changes nothing.
    pub(crate) fn margin_p50_at(&self, now_ms: u64) -> (i32, usize) {
        let horizon = now_ms.saturating_sub(FEEDBACK_WINDOW_MS);
        let mut margins: Vec<i32> = self
            .window
            .iter()
            .filter(|(at, _, _)| *at >= horizon)
            .map(|(_, m, _)| *m)
            .collect();
        if margins.is_empty() {
            return (0, 0);
        }
        margins.sort_unstable();
        let n = margins.len();
        (margins[(n - 1) / 2], n)
    }

    /// Takes the driver's total of missed ticks for this peer after a server tick.
    pub(crate) fn note_missed(&mut self, missed_total: u64) {
        if missed_total > self.last_missed_total {
            if self.active() {
                self.missing += missed_total - self.last_missed_total;
                self.urgent = true;
            }
            self.last_missed_total = missed_total;
        }
    }

    /// Whether an immediate record is due at `server_tick` (a late or missing input, and none sent
    /// on this tick yet).
    pub(crate) fn urgent_due(&self, server_tick: u64) -> bool {
        self.urgent && self.active() && self.last_sent_tick != Some(server_tick)
    }

    /// Inputs late and ticks missed so far (the quality tracker's input totals, ADR-8182).
    pub(crate) fn totals(&self) -> (u64, u64) {
        (self.late, self.missing)
    }

    /// Builds the record for now and marks it sent. `upstream_loss` is the loss the server's
    /// quality tracker measured on this peer's packets over the last 10 s (ADR-8182).
    pub(crate) fn feedback(
        &mut self,
        server_tick: u64,
        now_ms: u64,
        too_far_ahead: u64,
        input_buffer_ticks: usize,
        upstream_loss: f32,
    ) -> LinkFeedback {
        let horizon = now_ms.saturating_sub(FEEDBACK_WINDOW_MS);
        while self.window.front().is_some_and(|(at, _, _)| *at < horizon) {
            self.window.pop_front();
        }
        // Ticks before the server's next tick no longer matter; then every arrived tick extends.
        let mut c = self.contiguous.max(server_tick.checked_sub(1));
        let mut next = c.map_or(self.newest.map_or(0, |n| n.saturating_sub(SEEN_TICKS - 1)), |c| {
            c + 1
        });
        while self.is_seen(next) {
            c = Some(next);
            next += 1;
        }
        self.contiguous = c;
        self.scratch.clear();
        self.scratch.extend(self.window.iter().map(|(_, m, t)| (*m, *t)));
        self.scratch.sort_unstable();
        let n = self.scratch.len();
        let (p50, p5, p5_tick) = if n == 0 {
            (0, 0, None)
        } else {
            let (p5, tick) = self.scratch[(n - 1) * 5 / 100];
            (self.scratch[(n - 1) / 2].0, p5, Some(tick))
        };
        self.urgent = false;
        self.last_sent_tick = Some(server_tick);
        LinkFeedback {
            server_tick,
            newest_contiguous_tick: self.contiguous,
            margin_p50: p50,
            margin_p5: p5,
            margin_p5_tick: p5_tick,
            margin_samples: u32::try_from(n).unwrap_or(u32::MAX),
            inputs_late: self.late,
            inputs_missing: self.missing,
            inputs_too_far_ahead: too_far_ahead,
            upstream_loss,
            input_buffer_ticks: u32::try_from(input_buffer_ticks.max(1)).unwrap_or(u32::MAX),
        }
    }
}

/// Upstream loss from packet-number progress against packets received, per feedback interval:
/// the lockstep host's feedback (a `NetServer` peer's comes from its quality tracker, ADR-8889).
#[derive(Debug, Default)]
pub(crate) struct LossMeter {
    last: Option<(u16, u64)>,
    /// `(packet numbers spanned, packets received)` per interval, newest last.
    intervals: VecDeque<(u64, u64)>,
}

impl LossMeter {
    pub(crate) fn update(&mut self, remote_seq: Option<u16>, received: u64) -> f32 {
        let Some(seq) = remote_seq else {
            return 0.0;
        };
        if let Some((last_seq, last_received)) = self.last {
            let span = u64::from(seq.wrapping_sub(last_seq));
            // A wrap or reset larger than half the sequence space is not progress.
            if span < 1 << 15 {
                if self.intervals.len() >= LOSS_INTERVALS {
                    self.intervals.pop_front();
                }
                self.intervals
                    .push_back((span, received.saturating_sub(last_received)));
            }
        }
        self.last = Some((seq, received));
        let (span, got) = self
            .intervals
            .iter()
            .fold((0u64, 0u64), |(s, g), (a, b)| (s + a, g + b));
        if span == 0 {
            return 0.0;
        }
        (1.0 - (got.min(span) as f64 / span as f64)) as f32
    }
}

/// The client's server-clock estimate (spec 120 § 9.1).
///
/// Offsets are in milliseconds of server time minus local time. Each tick sync gives a sample
/// `tick * tick_ms - arrival`: queueing and jitter only ever make a sync arrive later, so the
/// largest sample of the window is the one with the smallest delay (the RFC 5905 § 10 clock filter
/// choice, for one-way samples). The target offset is that sample plus the one-way delay. The
/// presented estimate moves toward the target at `1 +- CLOCK_SLEW` times the local clock, so it
/// never runs backward; a target more than [`CLOCK_STEP_TICKS`] ahead steps forward at once.
#[derive(Debug, Clone)]
pub(crate) struct ClockFilter {
    tick_ms: f64,
    samples: VecDeque<f64>,
    target: Option<f64>,
    base_at: f64,
    base_value: f64,
    resyncs: u64,
}

impl ClockFilter {
    pub(crate) fn new(tick_rate_hz: u32) -> Self {
        Self {
            tick_ms: 1000.0 / f64::from(tick_rate_hz.max(1)),
            samples: VecDeque::with_capacity(CLOCK_FILTER_SAMPLES),
            target: None,
            base_at: 0.0,
            base_value: 0.0,
            resyncs: 0,
        }
    }

    /// Forward steps over [`CLOCK_STEP_TICKS`] after the warm-up.
    pub(crate) fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Presented server time, milliseconds, at local `now_ms`; `None` before the first sample.
    /// Non-decreasing in `now_ms`.
    pub(crate) fn server_ms(&self, now_ms: u64) -> Option<f64> {
        let target = self.target?;
        let now = (now_ms as f64).max(self.base_at);
        let elapsed = now - self.base_at;
        let goal = now + target;
        let at_rate = self.base_value + elapsed;
        Some(if at_rate <= goal {
            (self.base_value + elapsed * (1.0 + CLOCK_SLEW)).min(goal)
        } else {
            (self.base_value + elapsed * (1.0 - CLOCK_SLEW)).max(goal)
        })
    }

    /// Estimated server tick at `now_ms` (the tick an input sent now arrives in).
    pub(crate) fn tick(&self, now_ms: u64) -> u64 {
        self.server_ms(now_ms)
            .map_or(0, |ms| (ms / self.tick_ms).max(0.0).floor() as u64)
    }

    /// Takes one tick sync: server tick `tick` arrived at local `now_ms`, with `one_way_ms` the
    /// current one-way delay estimate.
    pub(crate) fn on_sync(&mut self, tick: u64, now_ms: u64, one_way_ms: f64) {
        let now = now_ms as f64;
        let warming_up = self.samples.len() < CLOCK_FILTER_SAMPLES;
        if !warming_up {
            self.samples.pop_front();
        }
        self.samples.push_back(tick as f64 * self.tick_ms - now);
        let best = self.samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let target = best + one_way_ms.max(0.0);
        let Some(current) = self.server_ms(now_ms) else {
            self.target = Some(target);
            self.base_at = now;
            self.base_value = now + target;
            return;
        };
        self.base_at = now;
        self.base_value = current;
        let ahead = now + target - current;
        if warming_up && ahead > 0.0 {
            // The first samples anchor the clock: the accept carries no delay estimate.
            self.base_value = now + target;
        } else if ahead > CLOCK_STEP_TICKS as f64 * self.tick_ms {
            self.base_value = now + target;
            self.resyncs += 1;
        }
        self.target = Some(target);
    }
}

/// The client's input lead (spec 120 § 9.2), steered from the server's [`LinkFeedback`].
///
/// The reported p5 margin covers inputs stamped over the last two seconds, some of them with an
/// older, smaller lead. So the controller corrects it by how much further ahead the client stamps
/// now than it did for the input that set the p5 (`stamp offset`: the stamped tick minus the local
/// clock in ticks; the record names that input's tick). Without the correction one late spell
/// would raise the lead once per record until the window turned over.
#[derive(Debug, Clone, Default)]
pub(crate) struct LeadController {
    lead: Option<u32>,
    calm_since: Option<u64>,
    /// `(tick, stamp offset)` of the first send of each stamped tick, oldest first.
    stamps: VecDeque<(u64, f64)>,
    last_late: u64,
    late_reported: bool,
}

impl LeadController {
    /// The steered lead; `None` before the first feedback with margin samples.
    pub(crate) fn lead(&self) -> Option<u32> {
        self.lead
    }

    /// Forgets everything (a new connection).
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Remembers the stamp offset of the first send of `tick`.
    pub(crate) fn note_stamp(&mut self, tick: u64, offset: f64) {
        if self.stamps.back().is_some_and(|(t, _)| *t >= tick) {
            return;
        }
        if self.stamps.len() >= MAX_STAMPS {
            self.stamps.pop_front();
        }
        self.stamps.push_back((tick, offset));
    }

    /// Takes one record. `fallback` is the lead in use before any feedback, `offset_now` the stamp
    /// offset the current lead gives, `window_ticks` [`FEEDBACK_WINDOW_MS`] in ticks and `max_lead`
    /// the clamp. Returns the margin to report when the inputs just went late (p5 below zero).
    pub(crate) fn on_feedback(
        &mut self,
        fb: &LinkFeedback,
        now_ms: u64,
        fallback: u32,
        offset_now: impl Fn(u32) -> f64,
        window_ticks: u64,
        max_lead: u32,
    ) -> Option<i32> {
        let late_event = if fb.margin_samples > 0 && fb.margin_p5 < 0 {
            (!std::mem::replace(&mut self.late_reported, true)).then_some(fb.margin_p5)
        } else {
            if fb.margin_samples > 0 {
                self.late_reported = false;
            }
            None
        };
        if fb.margin_samples == 0 {
            return late_event;
        }
        let lead = self.lead.unwrap_or(fallback).clamp(1, max_lead.max(1));
        // The stamp of the input that set the p5; without it (a stamp this client no longer
        // remembers), the lowest stamp offset of the window's ticks.
        let exact = fb.margin_p5_tick.and_then(|tick| {
            self.stamps
                .binary_search_by_key(&tick, |(t, _)| *t)
                .ok()
                .map(|i| self.stamps[i].1)
        });
        let worst = exact.unwrap_or_else(|| {
            let end = fb.newest_contiguous_tick.unwrap_or(fb.server_tick);
            let start = end.saturating_sub(window_ticks);
            self.stamps
                .iter()
                .filter(|(t, _)| *t > start && *t <= end)
                .map(|(_, o)| *o)
                .fold(f64::INFINITY, f64::min)
        });
        let gain = if worst.is_finite() {
            offset_now(lead) - worst
        } else {
            0.0
        };
        let measured = f64::from(fb.margin_p5);
        let corrected = measured + gain;
        let deficit = f64::from(TARGET_MARGIN_TICKS) - corrected;
        let new_late = fb.inputs_late > self.last_late;
        self.last_late = fb.inputs_late;
        // A lowering rests on a margin the server measured, not on a predicted one (ADR-10231):
        // a positive correction says the client stamps further ahead now than it did for the
        // p5 input (a raised lead, or a tick estimate that moved forward), so the spare tick is
        // only expected. The stamp offset follows the estimate, which wanders with a queue on
        // the way down; five seconds of expected spare time lowered the lead on
        // asym-bufferbloat and the next measured margin raised it again. A negative correction
        // (the lead came down since) still counts: the window's older inputs had more room.
        let spare = corrected.min(measured);
        let next = if deficit > 0.0 {
            self.calm_since = Some(now_ms);
            lead.saturating_add(deficit.ceil() as u32)
        } else if spare < f64::from(TARGET_MARGIN_TICKS + 1) || new_late {
            self.calm_since = Some(now_ms);
            lead
        } else {
            let since = *self.calm_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= LEAD_CALM_MS && lead > 1 {
                self.calm_since = Some(now_ms);
                lead - 1
            } else {
                lead
            }
        };
        self.lead = Some(next.clamp(1, max_lead.max(1)));
        late_event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_round_trips_and_saturates() {
        let fb = LinkFeedback {
            server_tick: 9_000,
            newest_contiguous_tick: Some(9_003),
            margin_p50: 3,
            margin_p5: -70_000,
            margin_p5_tick: Some(9_001),
            margin_samples: 70_000,
            inputs_late: 5,
            inputs_missing: u64::MAX,
            inputs_too_far_ahead: 2,
            upstream_loss: 0.0125,
            input_buffer_ticks: 64,
        };
        let mut w = WireWriter::new();
        fb.encode(&mut w);
        let bytes = w.finish();
        assert_eq!(bytes.len(), LINK_FEEDBACK_BYTES);
        let mut r = WireReader::new(&bytes);
        let back = LinkFeedback::decode(&mut r).expect("decodes");
        r.finish().expect("nothing after the record");
        assert_eq!(back.margin_p5, i32::from(i16::MIN));
        assert_eq!(back.margin_samples, u32::from(u16::MAX));
        assert_eq!(back.inputs_missing, u64::from(u32::MAX));
        assert_eq!(back.newest_contiguous_tick, Some(9_003));
        assert_eq!(back.margin_p5_tick, Some(9_001));
        assert!((back.upstream_loss - 0.0125).abs() < 1e-4);
        let none = LinkFeedback {
            newest_contiguous_tick: None,
            input_buffer_ticks: 1,
            ..LinkFeedback::default()
        };
        let mut w = WireWriter::new();
        none.encode(&mut w);
        let bytes = w.finish();
        assert_eq!(
            LinkFeedback::decode(&mut WireReader::new(&bytes)).expect("decodes"),
            none
        );
    }

    #[test]
    fn feedback_refuses_impossible_values() {
        let mut w = WireWriter::new();
        LinkFeedback {
            input_buffer_ticks: 64,
            ..LinkFeedback::default()
        }
        .encode(&mut w);
        let mut bytes = w.finish();
        let loss_at = LINK_FEEDBACK_BYTES - 4;
        bytes[loss_at..loss_at + 2].copy_from_slice(&10_001u16.to_le_bytes());
        assert!(LinkFeedback::decode(&mut WireReader::new(&bytes)).is_err());
        bytes[loss_at..loss_at + 2].copy_from_slice(&0u16.to_le_bytes());
        bytes[LINK_FEEDBACK_BYTES - 2..].copy_from_slice(&0u16.to_le_bytes());
        assert!(LinkFeedback::decode(&mut WireReader::new(&bytes)).is_err());
        assert!(LinkFeedback::decode(&mut WireReader::new(&bytes[..10])).is_err());
    }

    #[test]
    fn arrivals_count_first_copies_and_signed_margins() {
        let mut a = InputArrivals::default();
        assert!(!a.active());
        // Ticks 10..=12 arrive while the server is at 9 (margins 1, 2, 3), then a redundant copy.
        for t in 10..=12 {
            assert!(a.record(t, 9, 100));
        }
        assert!(!a.record(11, 9, 110), "a redundant copy is not a new arrival");
        // Tick 8 arrives after the server simulated it: late.
        assert!(a.record(8, 13, 120));
        let fb = a.feedback(13, 130, 4, 64, 0.0);
        assert_eq!(fb.margin_samples, 4);
        assert_eq!(fb.margin_p5, -5);
        assert_eq!(fb.margin_p5_tick, Some(8), "the late input set the p5");
        assert_eq!(fb.margin_p50, 1);
        assert_eq!(fb.inputs_late, 1);
        assert_eq!(fb.inputs_too_far_ahead, 4);
        assert_eq!(fb.newest_contiguous_tick, Some(12));
        // The window forgets arrivals older than two seconds.
        let fb = a.feedback(13, 130 + FEEDBACK_WINDOW_MS + 100, 4, 64, 0.0);
        assert_eq!(fb.margin_samples, 0);
    }

    #[test]
    fn contiguous_tick_waits_for_a_gap_until_the_server_passes_it() {
        let mut a = InputArrivals::default();
        for t in [3, 4, 5, 7] {
            a.record(t, 3, 0);
        }
        assert_eq!(a.feedback(3, 0, 0, 64, 0.0).newest_contiguous_tick, Some(5));
        a.record(6, 3, 0);
        assert_eq!(a.feedback(3, 0, 0, 64, 0.0).newest_contiguous_tick, Some(7));
        a.record(9, 3, 0);
        // Tick 8 never came; once the server simulated it, it no longer holds the run back.
        assert_eq!(a.feedback(9, 0, 0, 64, 0.0).newest_contiguous_tick, Some(9));
    }

    #[test]
    fn missing_ticks_make_a_record_due_once_per_tick() {
        let mut a = InputArrivals::default();
        a.note_missed(3);
        assert!(!a.urgent_due(1), "no record before the first input");
        a.record(4, 4, 0);
        assert!(!a.urgent_due(4));
        a.note_missed(4);
        assert!(a.urgent_due(5));
        let fb = a.feedback(5, 0, 0, 64, 0.0);
        assert_eq!(
            fb.inputs_missing, 1,
            "misses before the first input are not counted"
        );
        assert!(!a.urgent_due(5));
        a.note_missed(5);
        assert!(!a.urgent_due(5), "at most one immediate record per tick");
        assert!(a.urgent_due(6));
    }

    #[test]
    fn upstream_loss_comes_from_packet_number_progress() {
        let mut m = LossMeter::default();
        assert_eq!(m.update(Some(65_400), 50), 0.0);
        // 100 packet numbers, 90 received.
        let loss = m.update(Some(65_500), 140);
        assert!((loss - 0.1).abs() < 1e-6, "{loss}");
        // Across the u16 wrap: 100 more, all received.
        let loss = m.update(Some(64), 140 + 100);
        assert!((loss - 0.05).abs() < 1e-6, "{loss}");
        // A jump over half the sequence space is a reset, not progress.
        let loss = m.update(Some(40_000), 240);
        assert!((loss - 0.05).abs() < 1e-6, "{loss}");
    }

    #[test]
    fn the_clock_filter_takes_the_least_delayed_sync_and_never_runs_backward() {
        let mut c = ClockFilter::new(30);
        let tick_ms = 1000.0 / 30.0;
        // Server time = local time + 1000 ms; syncs every 250 ms with 20 ms base delay and a
        // queueing delay that alternates between 0 and 200 ms.
        let mut syncs = std::collections::BTreeMap::new();
        for i in 0..80u64 {
            let sent = 1_000 + i * 250;
            let delay = 20 + if i % 2 == 0 { 0 } else { 200 };
            syncs.insert(sent - 1000 + delay, (sent as f64 / tick_ms).floor() as u64);
        }
        let mut last = f64::NEG_INFINITY;
        for now in 0..20_250u64 {
            if let Some(tick) = syncs.get(&now) {
                c.on_sync(*tick, now, 20.0);
            }
            if let Some(ms) = c.server_ms(now) {
                assert!(ms >= last, "backward at {now}: {ms} < {last}");
                last = ms;
            }
        }
        // The estimate sits on the true server time (the least delayed syncs left on a tick edge).
        let err = c.server_ms(20_000).expect("anchored") - 21_000.0;
        assert!((-tick_ms..=1.0).contains(&err), "{err}");
        assert_eq!(c.resyncs(), 0);
    }

    #[test]
    fn the_clock_slews_by_at_most_five_percent_and_steps_only_far_forward() {
        let n = CLOCK_FILTER_SAMPLES as u64;
        let mut c = ClockFilter::new(60);
        for i in 0..n {
            c.on_sync(60 * i, 1000 * i, 0.0);
        }
        let at = 1000 * n;
        let before = c.server_ms(at).expect("anchored");
        // A target 5 ticks ahead slews: after 100 ms the estimate gained at most 105 ms.
        c.on_sync(60 * n + 5, at, 0.0);
        let after = c.server_ms(at + 100).expect("anchored");
        assert!(after - before <= 105.0 + 1e-9 && after - before > 100.0);
        assert_eq!(c.resyncs(), 0);
        // A target 20 ticks ahead steps at once and is counted.
        c.on_sync(60 * n + 40, at + 200, 0.0);
        assert!(c.server_ms(at + 200).expect("anchored") >= before + 200.0 + 20.0 * 1000.0 / 60.0);
        assert_eq!(c.resyncs(), 1);
        // A whole window of syncs one second behind never moves the estimate backward: it runs
        // at 95 % until it meets the target.
        let mut c2 = ClockFilter::new(60);
        for i in 0..n {
            c2.on_sync(60 * i, 1000 * i, 0.0);
        }
        let mut last = c2.server_ms(at).expect("anchored");
        for j in 0..n {
            let now = at + 250 * j;
            c2.on_sync(((now - 1000) as f64 * 0.06).floor() as u64, now, 0.0);
            for t in now..now + 250 {
                let v = c2.server_ms(t).expect("anchored");
                assert!(v >= last);
                last = v;
            }
        }
        let v0 = c2.server_ms(at + 2_000).expect("anchored");
        let v1 = c2.server_ms(at + 2_100).expect("anchored");
        assert!((95.0 - 1e-9..=100.0).contains(&(v1 - v0)), "{}", v1 - v0);
        assert_eq!(c2.resyncs(), 0);
    }

    fn fb(p5: i32, newest: u64, late: u64) -> LinkFeedback {
        LinkFeedback {
            server_tick: newest,
            newest_contiguous_tick: Some(newest),
            margin_p50: p5 + 2,
            margin_p5: p5,
            margin_samples: 60,
            inputs_late: late,
            input_buffer_ticks: 64,
            ..LinkFeedback::default()
        }
    }

    #[test]
    fn the_lead_rises_once_by_the_deficit_and_falls_after_a_calm_window() {
        let mut l = LeadController::default();
        // Stamps 100..160 with offset 2 (lead 2, the clock exact).
        for t in 100..160 {
            l.note_stamp(t, 2.0);
        }
        let offset = |lead: u32| f64::from(lead);
        let ev = l.on_feedback(&fb(-4, 159, 10), 0, 2, offset, 60, 62);
        assert_eq!(ev, Some(-4), "inputs just went late");
        assert_eq!(l.lead(), Some(7), "raised at once by the deficit 1 - (-4)");
        // The next record still holds the old samples: the correction keeps the lead.
        let ev = l.on_feedback(&fb(-4, 159, 12), 250, 2, offset, 60, 62);
        assert_eq!(ev, None, "reported once per late spell");
        assert_eq!(l.lead(), Some(7));
        // Fresh stamps at lead 7 arrive with margin 3: calm for five seconds lowers by one.
        for t in 160..400 {
            l.note_stamp(t, 7.0);
        }
        l.on_feedback(&fb(3, 399, 12), 1_000, 2, offset, 60, 62);
        assert_eq!(l.lead(), Some(7));
        // The calm window counts from the last record that was not calm (250 ms).
        l.on_feedback(&fb(3, 399, 12), 250 + LEAD_CALM_MS - 1, 2, offset, 60, 62);
        assert_eq!(l.lead(), Some(7));
        l.on_feedback(&fb(3, 399, 12), 250 + LEAD_CALM_MS, 2, offset, 60, 62);
        assert_eq!(l.lead(), Some(6));
        // A margin at the target holds the lead.
        l.on_feedback(&fb(1, 399, 12), 20_000, 2, |lead| f64::from(lead) + 1.0, 60, 62);
        assert_eq!(l.lead(), Some(6));
    }

    /// ADR-10231: a lowering rests on a margin the server measured. The client stamps a tick
    /// further ahead now than it did for the p5 input (its tick estimate moved forward), so a
    /// measured margin of 1 reads as a corrected 2: a spare tick that is only expected. Five
    /// seconds of that lowered the lead before (asym-bufferbloat: 18, 17 and back to 18 on the
    /// next measured margin); now the calm window waits for records that measure the spare tick.
    /// A negative correction still counts: after a lowering the window's older inputs had more
    /// room, and the lead keeps coming down one tick per calm window.
    #[test]
    fn an_expected_spare_tick_does_not_lower_the_lead_and_a_measured_one_does() {
        let mut l = LeadController::default();
        for t in 100..160 {
            l.note_stamp(t, 4.0);
        }
        // The estimate moved a tick forward since: lead 4 now stamps at offset 5.
        let ahead = |lead: u32| f64::from(lead) + 1.0;
        for now in (0..=2 * LEAD_CALM_MS).step_by(250) {
            l.on_feedback(&fb(1, 159, 0), now, 4, ahead, 60, 62);
            assert_eq!(
                l.lead(),
                Some(4),
                "at {now} ms: a measured margin of 1 at the target holds the lead"
            );
        }
        // Stamps at the new offset arrive with a measured margin of 2. The calm window counts
        // from the last record that was not calm.
        for t in 160..400 {
            l.note_stamp(t, 5.0);
        }
        let held = 2 * LEAD_CALM_MS;
        l.on_feedback(&fb(2, 399, 0), held + 250, 4, ahead, 60, 62);
        l.on_feedback(&fb(2, 399, 0), held + LEAD_CALM_MS - 1, 4, ahead, 60, 62);
        assert_eq!(l.lead(), Some(4));
        l.on_feedback(&fb(2, 399, 0), held + LEAD_CALM_MS, 4, ahead, 60, 62);
        assert_eq!(l.lead(), Some(3), "five measured calm seconds lower it by one");
        // The window still holds inputs stamped at lead 4 (offset 5; lead 3 stamps at 4): the
        // correction is -1, and a measured 3 is a corrected 2, calm again.
        let lowered = held + LEAD_CALM_MS;
        l.on_feedback(&fb(3, 399, 0), lowered + 250, 4, ahead, 60, 62);
        l.on_feedback(&fb(3, 399, 0), lowered + LEAD_CALM_MS - 1, 4, ahead, 60, 62);
        assert_eq!(l.lead(), Some(3));
        l.on_feedback(&fb(3, 399, 0), lowered + LEAD_CALM_MS, 4, ahead, 60, 62);
        assert_eq!(l.lead(), Some(2), "and by one more a calm window later");
    }

    /// The asym-bufferbloat start: a burst of ticks stamped late (offset 2) is still in the
    /// server's two-second window and sets the p5, while the stamps since are 33 ahead and the
    /// newest contiguous tick is past the burst by more than a window of ticks. The record names
    /// the p5 input's tick, so the lead holds instead of rising again.
    #[test]
    fn the_p5_input_tick_corrects_the_lead_exactly() {
        let mut l = LeadController::default();
        for t in 63..95 {
            l.note_stamp(t, 2.0);
        }
        for t in 95..170 {
            l.note_stamp(t, 33.0);
        }
        let offset = |lead: u32| f64::from(lead);
        let record = LinkFeedback {
            margin_p5_tick: Some(70),
            ..fb(-29, 169, 60)
        };
        l.on_feedback(&record, 0, 33, offset, 60, 62);
        assert_eq!(l.lead(), Some(33), "corrected p5 = -29 + (33 - 2) = 2: no raise");
        // The same record without the tick falls back to the window's ticks (110..=169), finds
        // only offset-33 stamps and raises by 30: the overshoot the tick removes.
        let mut l = LeadController::default();
        for t in 63..95 {
            l.note_stamp(t, 2.0);
        }
        for t in 95..170 {
            l.note_stamp(t, 33.0);
        }
        l.on_feedback(&fb(-29, 169, 60), 0, 33, offset, 60, 62);
        assert_eq!(l.lead(), Some(62));
    }

    #[test]
    fn the_lead_stays_inside_the_server_input_buffer() {
        let mut l = LeadController::default();
        l.on_feedback(&fb(-500, 10, 1), 0, 2, f64::from, 60, 30);
        assert_eq!(l.lead(), Some(30));
        let mut l = LeadController::default();
        l.on_feedback(&fb(50, 10, 0), 0, 0, f64::from, 60, 30);
        assert_eq!(l.lead(), Some(1), "never below one tick");
    }
}
