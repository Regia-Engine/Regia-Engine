// SPDX-License-Identifier: MPL-2.0
//! Per-link traffic meters and the stats / bandwidth views of the developer tools (spec 77 § 10.2).
//!
//! The loopback transport of the session layer ([`crate::session::hub`]) feeds one [`LinkMeter`]
//! per client: every message offered to a direction is counted by size and by message kind
//! ([`crate::session::proto::kind_name`]), lost messages are counted, and the delay the shaper
//! drew is smoothed into a round-trip estimate. All times come from the session clock, so a run
//! with a manual clock and a seeded conditioner reports the same numbers every time.
//!
//! [`ConnectionStats`] is one row of the stats overlay; [`BandwidthView`] is the per-kind byte
//! table. Both serialize for the agent methods `net.stats` and `net.bandwidth`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Width of one rate bucket in milliseconds.
const BUCKET_MS: u64 = 1000;
/// Delay samples the smoothing windows over (RFC 6298 gains, 1/8 and 1/4).
const SRTT_GAIN: f64 = 0.125;
const RTTVAR_GAIN: f64 = 0.25;

/// Counters of one traffic direction on one link.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DirMeter {
    /// Messages offered to the direction.
    pub packets: u64,
    /// Bytes offered to the direction.
    pub bytes: u64,
    /// Messages the shaper dropped.
    pub lost: u64,
    /// Smoothed one-way delay in milliseconds (0 until the first delivered message).
    pub delay_ms: f64,
    /// Smoothed variation of the one-way delay in milliseconds.
    pub jitter_ms: f64,
    /// Packets and bytes per message kind: `(packets, bytes)`.
    pub by_kind: BTreeMap<String, (u64, u64)>,
    /// The start of the current one-second bucket.
    bucket_start: u64,
    /// `(packets, bytes)` in the current bucket.
    bucket: (u64, u64),
    /// `(packets, bytes)` of the last full bucket.
    last_full: (u64, u64),
    samples: u64,
}

impl DirMeter {
    /// Records one message of `len` bytes and kind `kind` offered at `now_ms`; `delay_ms` is the
    /// earliest delivery delay the shaper drew, or `None` when the message was dropped.
    pub fn record(&mut self, kind: &str, len: usize, now_ms: u64, delay_ms: Option<f64>) {
        self.roll(now_ms);
        self.packets += 1;
        self.bytes += len as u64;
        self.bucket.0 += 1;
        self.bucket.1 += len as u64;
        let entry = self.by_kind.entry(kind.to_owned()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += len as u64;
        match delay_ms {
            None => self.lost += 1,
            Some(d) => {
                if self.samples == 0 {
                    self.delay_ms = d;
                    self.jitter_ms = d / 2.0;
                } else {
                    self.jitter_ms += RTTVAR_GAIN * ((self.delay_ms - d).abs() - self.jitter_ms);
                    self.delay_ms += SRTT_GAIN * (d - self.delay_ms);
                }
                self.samples += 1;
            }
        }
    }

    fn roll(&mut self, now_ms: u64) {
        let start = now_ms - now_ms % BUCKET_MS;
        if start == self.bucket_start {
            return;
        }
        // A gap of more than one bucket means the last full second saw nothing.
        self.last_full = if start == self.bucket_start + BUCKET_MS {
            self.bucket
        } else {
            (0, 0)
        };
        self.bucket = (0, 0);
        self.bucket_start = start;
    }

    /// Bytes per second over the last full one-second bucket as of `now_ms`.
    #[must_use]
    pub fn bytes_per_s(&self, now_ms: u64) -> f64 {
        self.rate(now_ms).1 as f64
    }

    /// Packets per second over the last full one-second bucket as of `now_ms`.
    #[must_use]
    pub fn packets_per_s(&self, now_ms: u64) -> f64 {
        self.rate(now_ms).0 as f64
    }

    fn rate(&self, now_ms: u64) -> (u64, u64) {
        let start = now_ms - now_ms % BUCKET_MS;
        if start == self.bucket_start {
            self.last_full
        } else if start == self.bucket_start + BUCKET_MS {
            self.bucket
        } else {
            (0, 0)
        }
    }

    /// Fraction of offered messages that were dropped (0 with no traffic).
    #[must_use]
    pub fn loss_fraction(&self) -> f32 {
        if self.packets == 0 {
            0.0
        } else {
            self.lost as f32 / self.packets as f32
        }
    }
}

/// Both directions of one link: `up` is client → server, `down` is server → client.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkMeter {
    /// Client → server.
    pub up: DirMeter,
    /// Server → client.
    pub down: DirMeter,
}

impl LinkMeter {
    /// Smoothed round-trip time: the one-way delays of both directions added.
    #[must_use]
    pub fn rtt_ms(&self) -> f64 {
        self.up.delay_ms + self.down.delay_ms
    }

    /// Round-trip variation: the one-way variations added.
    #[must_use]
    pub fn jitter_ms(&self) -> f64 {
        self.up.jitter_ms + self.down.jitter_ms
    }
}

/// One row of the stats overlay (spec 77 § 10.2): one connection as the server sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionStats {
    /// Display label (`client-1`, `sim-3`).
    pub label: String,
    /// Peer id once welcomed.
    pub peer: Option<u32>,
    /// Session state name (`in_game`, `connecting`, ...).
    pub state: String,
    /// True for clients made by `net.sim_clients`.
    pub simulated: bool,
    /// Smoothed round-trip time, milliseconds.
    pub rtt_ms: f64,
    /// Round-trip variation, milliseconds.
    pub jitter_ms: f64,
    /// Fraction of client → server messages lost.
    pub loss_out: f32,
    /// Fraction of server → client messages lost.
    pub loss_in: f32,
    /// Bytes per second sent by the client.
    pub bytes_out_per_s: f64,
    /// Bytes per second received by the client.
    pub bytes_in_per_s: f64,
    /// Packets per second, both directions.
    pub packets_per_s: f64,
    /// Total bytes sent by the client.
    pub bytes_out: u64,
    /// Total bytes received by the client.
    pub bytes_in: u64,
    /// Prediction corrections per second (spec 73); `None` while no prediction state is attached.
    pub corrections_per_s: Option<f64>,
    /// Interpolation delay in milliseconds (spec 74); `None` while no interpolation is attached.
    pub interp_delay_ms: Option<f64>,
}

impl ConnectionStats {
    /// Builds a row from a link's meter at `now_ms`.
    #[must_use]
    pub fn from_meter(
        label: &str,
        peer: Option<u32>,
        state: &str,
        simulated: bool,
        meter: &LinkMeter,
        now_ms: u64,
    ) -> Self {
        Self {
            label: label.to_owned(),
            peer,
            state: state.to_owned(),
            simulated,
            rtt_ms: meter.rtt_ms(),
            jitter_ms: meter.jitter_ms(),
            loss_out: meter.up.loss_fraction(),
            loss_in: meter.down.loss_fraction(),
            bytes_out_per_s: meter.up.bytes_per_s(now_ms),
            bytes_in_per_s: meter.down.bytes_per_s(now_ms),
            packets_per_s: meter.up.packets_per_s(now_ms) + meter.down.packets_per_s(now_ms),
            bytes_out: meter.up.bytes,
            bytes_in: meter.down.bytes,
            corrections_per_s: None,
            interp_delay_ms: None,
        }
    }

    /// The overlay text line.
    #[must_use]
    pub fn line(&self) -> String {
        let mut s = format!(
            "{:<10} {:<10} rtt {:>5.0}±{:<3.0} ms loss {:>4.1}/{:<4.1} % out {:>7.0} B/s in {:>7.0} B/s {:>5.0} pkt/s",
            self.label,
            self.state,
            self.rtt_ms,
            self.jitter_ms,
            self.loss_out * 100.0,
            self.loss_in * 100.0,
            self.bytes_out_per_s,
            self.bytes_in_per_s,
            self.packets_per_s
        );
        if let Some(c) = self.corrections_per_s {
            s.push_str(&format!(" corr {c:.1}/s"));
        }
        if let Some(d) = self.interp_delay_ms {
            s.push_str(&format!(" interp {d:.0} ms"));
        }
        s
    }
}

/// One row of the bandwidth view: a message kind and its bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindBytes {
    /// Message kind (`game`, `stream_chunk`, ...).
    pub kind: String,
    /// Packets sent by clients.
    pub packets_out: u64,
    /// Bytes sent by clients.
    pub bytes_out: u64,
    /// Packets sent by the server.
    pub packets_in: u64,
    /// Bytes sent by the server.
    pub bytes_in: u64,
    /// Share of all bytes, 0..=1.
    pub share: f64,
}

/// The per-message-kind bandwidth table of a set of links (spec 77 § 10.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BandwidthView {
    /// Rows, largest first (ties by name).
    pub kinds: Vec<KindBytes>,
    /// All bytes sent by clients.
    pub total_out: u64,
    /// All bytes sent by the server.
    pub total_in: u64,
}

impl BandwidthView {
    /// Sums the meters of `links` by message kind.
    pub fn from_meters<'a>(links: impl IntoIterator<Item = &'a LinkMeter>) -> Self {
        let mut map: BTreeMap<String, KindBytes> = BTreeMap::new();
        let mut view = Self::default();
        for m in links {
            for (kind, (p, b)) in &m.up.by_kind {
                let row = map.entry(kind.clone()).or_insert_with(|| empty_row(kind));
                row.packets_out += p;
                row.bytes_out += b;
            }
            for (kind, (p, b)) in &m.down.by_kind {
                let row = map.entry(kind.clone()).or_insert_with(|| empty_row(kind));
                row.packets_in += p;
                row.bytes_in += b;
            }
            view.total_out += m.up.bytes;
            view.total_in += m.down.bytes;
        }
        let total = (view.total_out + view.total_in).max(1) as f64;
        let mut rows: Vec<KindBytes> = map.into_values().collect();
        for r in &mut rows {
            r.share = (r.bytes_out + r.bytes_in) as f64 / total;
        }
        rows.sort_by(|a, b| {
            (b.bytes_out + b.bytes_in)
                .cmp(&(a.bytes_out + a.bytes_in))
                .then_with(|| a.kind.cmp(&b.kind))
        });
        view.kinds = rows;
        view
    }

    /// One text line per kind plus a header, for the overlay.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "bandwidth: out {} B, in {} B",
            self.total_out, self.total_in
        )];
        for k in &self.kinds {
            out.push(format!(
                "  {:<14} out {:>8} B ({:>5} pkt) in {:>8} B ({:>5} pkt) {:>5.1} %",
                k.kind,
                k.bytes_out,
                k.packets_out,
                k.bytes_in,
                k.packets_in,
                k.share * 100.0
            ));
        }
        out
    }
}

fn empty_row(kind: &str) -> KindBytes {
    KindBytes {
        kind: kind.to_owned(),
        packets_out: 0,
        bytes_out: 0,
        packets_in: 0,
        bytes_in: 0,
        share: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_use_the_last_full_second() {
        let mut d = DirMeter::default();
        d.record("game", 100, 10, Some(5.0));
        d.record("game", 50, 900, Some(5.0));
        // Still inside the first bucket: nothing is full yet.
        assert_eq!(d.bytes_per_s(950), 0.0);
        d.record("game", 10, 1100, Some(5.0));
        assert_eq!(d.bytes_per_s(1200), 150.0);
        assert_eq!(d.packets_per_s(1200), 2.0);
        // Two seconds of silence: the last full second was empty.
        assert_eq!(d.bytes_per_s(3500), 0.0);
        assert_eq!(d.bytes, 160);
    }

    #[test]
    fn loss_and_delay_are_counted() {
        let mut d = DirMeter::default();
        d.record("game", 10, 0, Some(40.0));
        d.record("game", 10, 1, None);
        d.record("game", 10, 2, Some(60.0));
        d.record("keepalive", 1, 3, None);
        assert_eq!(d.lost, 2);
        assert!((d.loss_fraction() - 0.5).abs() < 1e-6);
        assert!(d.delay_ms > 40.0 && d.delay_ms < 60.0);
        assert_eq!(d.by_kind["game"], (3, 30));
        assert_eq!(d.by_kind["keepalive"], (1, 1));
    }

    #[test]
    fn bandwidth_view_sorts_and_shares() {
        let mut a = LinkMeter::default();
        a.up.record("game", 10, 0, Some(1.0));
        a.down.record("join_records", 90, 0, Some(1.0));
        let mut b = LinkMeter::default();
        b.down.record("game", 100, 0, Some(1.0));
        let v = BandwidthView::from_meters([&a, &b]);
        assert_eq!(v.total_out, 10);
        assert_eq!(v.total_in, 190);
        assert_eq!(v.kinds[0].kind, "game");
        assert_eq!(v.kinds[0].bytes_out + v.kinds[0].bytes_in, 110);
        assert_eq!(v.kinds[1].kind, "join_records");
        let sum: f64 = v.kinds.iter().map(|k| k.share).sum();
        assert!((sum - 1.0).abs() < 1e-9);
        assert_eq!(v.lines().len(), 3);
    }

    #[test]
    fn rtt_adds_both_directions() {
        let mut m = LinkMeter::default();
        m.up.record("game", 1, 0, Some(30.0));
        m.down.record("game", 1, 0, Some(20.0));
        assert_eq!(m.rtt_ms(), 50.0);
    }
}
