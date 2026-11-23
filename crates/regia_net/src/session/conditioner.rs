// SPDX-License-Identifier: MPL-2.0
//! The session network conditioner (spec 77 § 10.1): per-direction latency, jitter, loss,
//! duplication, reordering and a bandwidth cap, with every random draw from a seeded generator so
//! a run with the same traffic is repeatable.
//!
//! Order of effects per packet: loss, then delay plus jitter, then the reorder extra delay, then
//! duplication (the copy takes its own delay draw), then the bandwidth bucket. Dev builds only:
//! shipping builds (`debug_assertions` off) leave it out of the agent surface;
//! [`conditioner_available`] says which. The shaper itself (`LinkShaper`) is compiled out of
//! those builds too (spec 77 § 10.1, ADR-7212): the settings types stay so configs and the agent
//! schema read the same everywhere, and the loopback hub passes packets through unshaped.
//! [`LINK_SHAPER_COMPILED`] says whether this build has it; `tests/shipping_no_link_shaper.rs`
//! is the compile check.

use serde::{Deserialize, Serialize};

#[cfg(debug_assertions)]
use crate::testing::SplitMix64;

/// Conditioner preset names.
pub const SESSION_CONDITIONER_PRESETS: &[&str] = &["off", "lan", "good", "average", "bad", "terrible"];

/// True in builds that carry the conditioner's agent surface (dev builds).
pub const fn conditioner_available() -> bool {
    cfg!(debug_assertions)
}

/// True in builds that carry `LinkShaper` (dev builds, `debug_assertions` on). Shipping builds
/// compile it out (spec 77 § 10.1).
pub const LINK_SHAPER_COMPILED: bool = cfg!(debug_assertions);

/// One direction's conditions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShaperSettings {
    /// Base one-way latency, milliseconds.
    pub latency_ms: u32,
    /// Uniform ± jitter around the latency, milliseconds.
    pub jitter_ms: u32,
    /// Loss, percent.
    pub loss_pct: f64,
    /// Duplication, percent.
    pub duplicate_pct: f64,
    /// Reorder, percent: a chosen packet gets `reorder_extra_ms` more delay.
    pub reorder_pct: f64,
    /// Extra delay of a reordered packet, milliseconds.
    pub reorder_extra_ms: u32,
    /// Bandwidth, bytes per second (0 = no cap).
    pub bandwidth_bytes_per_s: u32,
}

impl Default for ShaperSettings {
    fn default() -> Self {
        Self {
            latency_ms: 0,
            jitter_ms: 0,
            loss_pct: 0.0,
            duplicate_pct: 0.0,
            reorder_pct: 0.0,
            reorder_extra_ms: 20,
            bandwidth_bytes_per_s: 0,
        }
    }
}

impl ShaperSettings {
    /// A preset by name (`off`, `lan`, `good`, `average`, `bad`, `terrible`).
    pub fn preset(name: &str) -> Option<Self> {
        let base = Self::default();
        Some(match name {
            "off" => base,
            "lan" => Self {
                latency_ms: 1,
                ..base
            },
            "good" => Self {
                latency_ms: 30,
                jitter_ms: 2,
                loss_pct: 0.1,
                ..base
            },
            "average" => Self {
                latency_ms: 60,
                jitter_ms: 8,
                loss_pct: 1.0,
                ..base
            },
            "bad" => Self {
                latency_ms: 120,
                jitter_ms: 20,
                loss_pct: 3.0,
                reorder_pct: 1.0,
                ..base
            },
            "terrible" => Self {
                latency_ms: 250,
                jitter_ms: 50,
                loss_pct: 8.0,
                duplicate_pct: 2.0,
                reorder_pct: 3.0,
                ..base
            },
            _ => return None,
        })
    }

    /// Clamps percentages to 0..=100 and delays to 60 s.
    pub fn sanitized(mut self) -> Self {
        let pct = |v: f64| if v.is_finite() { v.clamp(0.0, 100.0) } else { 0.0 };
        self.loss_pct = pct(self.loss_pct);
        self.duplicate_pct = pct(self.duplicate_pct);
        self.reorder_pct = pct(self.reorder_pct);
        self.latency_ms = self.latency_ms.min(60_000);
        self.jitter_ms = self.jitter_ms.min(self.latency_ms.max(1)).min(60_000);
        self.reorder_extra_ms = self.reorder_extra_ms.min(60_000);
        self
    }
}

/// Both directions plus the seed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConditioner {
    /// Packets this process sends.
    pub outbound: ShaperSettings,
    /// Packets this process receives.
    pub inbound: ShaperSettings,
    /// RNG seed.
    pub seed: u64,
}

impl Default for SessionConditioner {
    fn default() -> Self {
        Self {
            outbound: ShaperSettings::default(),
            inbound: ShaperSettings::default(),
            seed: 0x5eed,
        }
    }
}

impl SessionConditioner {
    /// The same preset both ways.
    pub fn preset(name: &str) -> Option<Self> {
        let s = ShaperSettings::preset(name)?;
        Some(Self {
            outbound: s,
            inbound: s,
            seed: 0x5eed,
        })
    }
}

/// Counters of one shaper.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ShaperStats {
    /// Packets offered.
    pub offered: u64,
    /// Packets dropped.
    pub lost: u64,
    /// Extra copies made.
    pub duplicated: u64,
    /// Packets given the reorder delay.
    pub reordered: u64,
}

/// One direction's shaper: turns "send `len` bytes at `now`" into zero, one or two release times.
/// Dev builds only: compiled out of builds without `debug_assertions` (spec 77 § 10.1).
#[cfg(debug_assertions)]
#[derive(Debug, Clone)]
pub struct LinkShaper {
    settings: ShaperSettings,
    rng: SplitMix64,
    bucket_free_at: f64,
    stats: ShaperStats,
}

#[cfg(debug_assertions)]
impl LinkShaper {
    /// A shaper with its seed.
    pub fn new(settings: ShaperSettings, seed: u64) -> Self {
        Self {
            settings: settings.sanitized(),
            rng: SplitMix64::new(seed),
            bucket_free_at: 0.0,
            stats: ShaperStats::default(),
        }
    }

    /// The settings.
    pub fn settings(&self) -> &ShaperSettings {
        &self.settings
    }

    /// The counters.
    pub fn stats(&self) -> ShaperStats {
        self.stats
    }

    fn chance(&mut self, pct: f64) -> bool {
        pct > 0.0 && self.rng.next_f64() * 100.0 < pct
    }

    fn delay(&mut self) -> f64 {
        let s = self.settings;
        let jitter = if s.jitter_ms > 0 {
            (self.rng.next_f64() * 2.0 - 1.0) * f64::from(s.jitter_ms)
        } else {
            0.0
        };
        (f64::from(s.latency_ms) + jitter).max(0.0)
    }

    /// Whether a lost packet is dropped (`droppable`) or only delayed by resends (reliable).
    /// Returns release times in milliseconds, ascending by draw order (not sorted).
    pub fn shape(&mut self, len: usize, now_ms: u64, droppable: bool) -> Vec<u64> {
        self.stats.offered += 1;
        let now = now_ms as f64;
        let mut resend_extra = 0.0;
        // 1. Loss. A reliable packet is resent after a round trip until one copy survives.
        // At most 64 resends: a reliable packet always arrives, even at 100 % loss.
        let mut resends = 0;
        while self.chance(self.settings.loss_pct) {
            self.stats.lost += 1;
            if droppable {
                return Vec::new();
            }
            resend_extra += (2.0 * f64::from(self.settings.latency_ms)).max(50.0);
            resends += 1;
            if resends >= 64 {
                break;
            }
        }
        // 2. Delay plus jitter.
        let mut first = now + resend_extra + self.delay();
        // 3. Reorder extra delay.
        if self.chance(self.settings.reorder_pct) {
            self.stats.reordered += 1;
            first += f64::from(self.settings.reorder_extra_ms);
        }
        let mut times = vec![first];
        // 4. Duplication (only for packets that may arrive twice); the copy draws its own delay.
        if droppable && self.chance(self.settings.duplicate_pct) {
            self.stats.duplicated += 1;
            times.push(now + resend_extra + self.delay());
        }
        // 5. Bandwidth bucket: packets leave one after another at the cap.
        if self.settings.bandwidth_bytes_per_s > 0 {
            let cost = len as f64 * 1000.0 / f64::from(self.settings.bandwidth_bytes_per_s);
            for t in &mut times {
                let start = self.bucket_free_at.max(now);
                self.bucket_free_at = start + cost;
                *t += start - now + cost;
            }
        }
        times.into_iter().map(|t| t.ceil() as u64).collect()
    }
}

/// What the loopback hub uses in place of `LinkShaper` in builds without it: every packet
/// leaves at once, unshaped.
#[cfg(not(debug_assertions))]
#[derive(Debug, Clone)]
pub(crate) struct PassThroughShaper;

#[cfg(not(debug_assertions))]
impl PassThroughShaper {
    /// The same constructor as `LinkShaper`; the settings and seed are ignored.
    pub(crate) fn new(_settings: ShaperSettings, _seed: u64) -> Self {
        Self
    }

    /// One release time: now.
    pub(crate) fn shape(&mut self, _len: usize, now_ms: u64, _droppable: bool) -> Vec<u64> {
        vec![now_ms]
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn presets_and_rates() {
        for name in SESSION_CONDITIONER_PRESETS {
            assert!(ShaperSettings::preset(name).is_some(), "{name}");
        }
        assert!(ShaperSettings::preset("nope").is_none());
        let s = ShaperSettings {
            loss_pct: 5.0,
            duplicate_pct: 3.0,
            ..ShaperSettings::default()
        };
        let mut shaper = LinkShaper::new(s, 42);
        let n = 100_000u64;
        let mut delivered = 0u64;
        for i in 0..n {
            delivered += shaper.shape(100, i, true).len() as u64;
        }
        let st = shaper.stats();
        let loss = st.lost as f64 / n as f64 * 100.0;
        let dup = st.duplicated as f64 / (n - st.lost) as f64 * 100.0;
        assert!((loss - 5.0).abs() <= 1.0, "loss {loss}");
        assert!((dup - 3.0).abs() <= 1.0, "dup {dup}");
        assert_eq!(delivered, n - st.lost + st.duplicated);
    }

    #[test]
    fn same_seed_same_log_and_latency_applies() {
        let s = ShaperSettings::preset("bad").expect("preset");
        let run = |seed| {
            let mut sh = LinkShaper::new(s, seed);
            (0..1000u64)
                .map(|i| sh.shape(64, i * 16, true))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
        let mut lat = LinkShaper::new(
            ShaperSettings {
                latency_ms: 100,
                ..ShaperSettings::default()
            },
            1,
        );
        assert_eq!(lat.shape(10, 1000, true), vec![1100]);
    }

    /// Review regression: 100 % loss never hangs a reliable packet; it arrives after capped resends.
    #[test]
    fn full_loss_still_delivers_reliable_packets() {
        let mut sh = LinkShaper::new(
            ShaperSettings {
                loss_pct: 100.0,
                ..ShaperSettings::default()
            },
            1,
        );
        assert!(sh.shape(10, 0, true).is_empty());
        assert_eq!(sh.shape(10, 0, false).len(), 1);
    }

    #[test]
    fn bandwidth_bucket_delays() {
        let mut sh = LinkShaper::new(
            ShaperSettings {
                bandwidth_bytes_per_s: 1000,
                ..ShaperSettings::default()
            },
            1,
        );
        assert_eq!(sh.shape(100, 0, true), vec![100]);
        assert_eq!(sh.shape(100, 0, true), vec![200]);
        assert_eq!(sh.shape(100, 1000, true), vec![1100]);
    }

    #[test]
    fn reorder_lets_later_packets_overtake() {
        let mut sh = LinkShaper::new(
            ShaperSettings {
                latency_ms: 10,
                reorder_pct: 100.0,
                reorder_extra_ms: 20,
                ..ShaperSettings::default()
            },
            1,
        );
        assert_eq!(sh.shape(1, 0, true), vec![30]);
    }
}
