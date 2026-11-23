// SPDX-License-Identifier: MPL-2.0
//! Network conditioner: seeded loss, duplication, delay, jitter, reorder and loss bursts.
//!
//! Every outbound datagram of a server or client passes through one [`NetConditioner`]. Disabled
//! (the default) it is a pass-through. Enabled, it makes decisions from one seeded xorshift64
//! stream consumed in call order, and time is passed in as milliseconds, so the same seed and the
//! same traffic replay the same drops, holds and release order on every machine. Tests use it in
//! place of `tc netem`; the agent method `net.conditioner_set` changes it at run time.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Conditioner parameters. All probabilities are in `[0, 1]`; values outside are clamped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConditionerConfig {
    /// Master switch; when `false` the conditioner passes every datagram through untouched.
    pub enabled: bool,
    /// PRNG seed; the same seed replays the same decisions for the same traffic.
    pub seed: u64,
    /// Probability that a datagram is dropped.
    pub loss: f64,
    /// Probability that a datagram is sent twice.
    pub duplicate: f64,
    /// Probability that a datagram gets an extra hold of up to `reorder_max_ms` (a reorder event).
    pub reorder: f64,
    /// Upper bound of the extra reorder hold, in milliseconds.
    pub reorder_max_ms: u64,
    /// Constant one-way delay added to every datagram that is not dropped, in milliseconds.
    pub delay_ms: u64,
    /// Extra hold, uniform in `0..=jitter_ms`, on top of `delay_ms`.
    pub jitter_ms: u64,
    /// Start of a loss burst, in milliseconds after the first conditioned datagram. `0` disables
    /// the burst.
    pub burst_start_ms: u64,
    /// Length of the burst window in milliseconds.
    pub burst_len_ms: u64,
    /// Drop probability inside the burst window (`1.0` is total loss).
    pub burst_loss: f64,
}

impl Default for ConditionerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            seed: 1,
            loss: 0.0,
            duplicate: 0.0,
            reorder: 0.0,
            reorder_max_ms: 0,
            delay_ms: 0,
            jitter_ms: 0,
            burst_start_ms: 0,
            burst_len_ms: 0,
            burst_loss: 1.0,
        }
    }
}

/// Largest delay, jitter, reorder or burst time the conditioner accepts (one minute).
pub const MAX_CONDITIONER_MS: u64 = 60_000;
/// Most datagrams the conditioner holds at once; beyond it new holds are dropped (and counted).
pub const MAX_HELD_DATAGRAMS: usize = 65_536;

/// Names accepted by [`ConditionerConfig::from_profile`].
pub const CONDITIONER_PROFILES: &[&str] = &["off", "lan", "home", "wan", "wan_stress", "bad", "burst"];

impl ConditionerConfig {
    /// A profile by name: `off`/`lan` (pass-through), `home` (30 ms one way, 0.5 % loss),
    /// `wan` (60 ms plus up to 10 ms, 2 % loss, 1 % duplicates, 5 % reorders up to 40 ms),
    /// `wan_stress` (80 ms plus up to 20 ms, 8 % loss, 2 % duplicates, 15 % reorders up to 75 ms),
    /// `bad` (120 ms plus up to 60 ms, 5 % loss, 1 % reorders) and `burst` (`wan` plus a
    /// two-second total-loss burst ten seconds in). Jitter is added on top of the delay.
    pub fn from_profile(name: &str) -> Option<Self> {
        let on = |seed: u64| Self {
            enabled: true,
            seed,
            ..Self::default()
        };
        Some(match name {
            "off" | "lan" => Self::default(),
            "home" => Self {
                delay_ms: 30,
                loss: 0.005,
                ..on(1)
            },
            "wan" => Self {
                delay_ms: 60,
                jitter_ms: 10,
                loss: 0.02,
                duplicate: 0.01,
                reorder: 0.05,
                reorder_max_ms: 40,
                ..on(1)
            },
            "wan_stress" => Self {
                delay_ms: 80,
                jitter_ms: 20,
                loss: 0.08,
                duplicate: 0.02,
                reorder: 0.15,
                reorder_max_ms: 75,
                ..on(1)
            },
            "bad" => Self {
                delay_ms: 120,
                jitter_ms: 60,
                loss: 0.05,
                reorder: 0.01,
                reorder_max_ms: 30,
                ..on(1)
            },
            "burst" => Self {
                burst_start_ms: 10_000,
                burst_len_ms: 2_000,
                burst_loss: 1.0,
                ..Self::from_profile("wan")?
            },
            _ => return None,
        })
    }

    /// Clamps every probability into `[0, 1]` (NaN becomes 0) and every time into
    /// `0..=`[`MAX_CONDITIONER_MS`].
    pub fn sanitized(mut self) -> Self {
        let clamp = |p: f64| if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
        self.loss = clamp(self.loss);
        self.duplicate = clamp(self.duplicate);
        self.reorder = clamp(self.reorder);
        self.burst_loss = clamp(self.burst_loss);
        for ms in [
            &mut self.reorder_max_ms,
            &mut self.delay_ms,
            &mut self.jitter_ms,
            &mut self.burst_start_ms,
            &mut self.burst_len_ms,
        ] {
            *ms = (*ms).min(MAX_CONDITIONER_MS);
        }
        self
    }
}

/// Decision counters since the conditioner was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionerStats {
    /// Datagrams released (immediately or after a hold), duplicates included.
    pub passed: u64,
    /// Datagrams dropped (random loss and burst loss).
    pub dropped: u64,
    /// Datagrams sent twice.
    pub duplicated: u64,
    /// Datagrams held for delay, jitter or reorder.
    pub delayed: u64,
}

/// A datagram ready for the socket: bytes and destination.
pub type Datagram = (Vec<u8>, SocketAddr);

#[derive(Debug)]
struct Held {
    release_ms: u64,
    order: u64,
    datagram: Datagram,
}

/// Deterministic outbound datagram conditioner.
///
/// Call [`NetConditioner::process`] for every outbound datagram and [`NetConditioner::release_due`]
/// once per update. It owns no thread or timer.
#[derive(Debug)]
pub struct NetConditioner {
    config: ConditionerConfig,
    rng: u64,
    start_ms: Option<u64>,
    held: Vec<Held>,
    next_order: u64,
    stats: ConditionerStats,
}

impl NetConditioner {
    /// A conditioner with `config`.
    pub fn new(config: ConditionerConfig) -> Self {
        let config = config.sanitized();
        Self {
            rng: seed_state(config.seed),
            config,
            start_ms: None,
            held: Vec::new(),
            next_order: 0,
            stats: ConditionerStats::default(),
        }
    }

    /// Replaces the configuration and reseeds. Datagrams already held keep their release time.
    pub fn set_config(&mut self, config: ConditionerConfig) {
        let config = config.sanitized();
        self.rng = seed_state(config.seed);
        self.config = config;
        self.start_ms = None;
    }

    /// The active configuration.
    pub fn config(&self) -> &ConditionerConfig {
        &self.config
    }

    /// Decision counters.
    pub fn stats(&self) -> ConditionerStats {
        self.stats
    }

    /// Datagrams currently held.
    pub fn pending(&self) -> usize {
        self.held.len()
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64 (Marsaglia, "Xorshift RNGs", Journal of Statistical Software 8(14), 2003).
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn next_fraction(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn in_burst(&self, now_ms: u64) -> bool {
        let Some(start) = self.start_ms else {
            return false;
        };
        if self.config.burst_start_ms == 0 || self.config.burst_len_ms == 0 {
            return false;
        }
        let t = now_ms.saturating_sub(start);
        t >= self.config.burst_start_ms
            && t < self
                .config
                .burst_start_ms
                .saturating_add(self.config.burst_len_ms)
    }

    fn hold(&mut self, release_ms: u64, datagram: Datagram) {
        if self.held.len() >= MAX_HELD_DATAGRAMS {
            self.stats.dropped += 1;
            return;
        }
        self.held.push(Held {
            release_ms,
            order: self.next_order,
            datagram,
        });
        self.next_order += 1;
    }

    /// The conditioner is off: [`Self::process`] hands every datagram straight back, at once.
    pub(crate) fn passes_through(&self) -> bool {
        !self.config.enabled
    }

    /// Conditions one datagram. Returns what to send now (empty on a drop or a hold, two entries
    /// on a duplicate). Held datagrams come out of [`NetConditioner::release_due`].
    pub fn process(&mut self, bytes: Vec<u8>, dest: SocketAddr, now_ms: u64) -> Vec<Datagram> {
        if !self.config.enabled {
            self.stats.passed += 1;
            return vec![(bytes, dest)];
        }
        if self.start_ms.is_none() {
            self.start_ms = Some(now_ms);
        }
        if self.next_fraction() < self.config.loss {
            self.stats.dropped += 1;
            return Vec::new();
        }
        if self.in_burst(now_ms) && self.next_fraction() < self.config.burst_loss {
            self.stats.dropped += 1;
            return Vec::new();
        }
        let duplicate = self.next_fraction() < self.config.duplicate;
        let jitter = if self.config.jitter_ms == 0 {
            0
        } else {
            self.next_u64() % (self.config.jitter_ms + 1)
        };
        let mut hold_ms = self.config.delay_ms.saturating_add(jitter);
        if self.config.reorder > 0.0
            && self.config.reorder_max_ms > 0
            && self.next_fraction() < self.config.reorder
        {
            hold_ms = hold_ms.saturating_add(1 + self.next_u64() % self.config.reorder_max_ms);
        }
        if duplicate {
            self.stats.duplicated += 1;
        }
        if hold_ms == 0 {
            self.stats.passed += 1;
            if duplicate {
                self.stats.passed += 1;
                return vec![(bytes.clone(), dest), (bytes, dest)];
            }
            return vec![(bytes, dest)];
        }
        self.stats.delayed += 1;
        let release = now_ms.saturating_add(hold_ms);
        if duplicate {
            self.hold(release, (bytes.clone(), dest));
        }
        self.hold(release, (bytes, dest));
        Vec::new()
    }

    /// Releases every held datagram whose time has come, earliest first (ties in hold order).
    pub fn release_due(&mut self, now_ms: u64) -> Vec<Datagram> {
        if self.held.is_empty() {
            return Vec::new();
        }
        let mut due: Vec<Held> = Vec::new();
        let mut index = 0;
        while index < self.held.len() {
            if self.held[index].release_ms <= now_ms {
                due.push(self.held.swap_remove(index));
            } else {
                index += 1;
            }
        }
        due.sort_by_key(|h| (h.release_ms, h.order));
        self.stats.passed += due.len() as u64;
        due.into_iter().map(|h| h.datagram).collect()
    }
}

fn seed_state(seed: u64) -> u64 {
    if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 9999))
    }

    /// Feeds `packets` datagrams (one per millisecond) and returns the arrival order of their
    /// sequence numbers, holds included, plus the counters.
    fn run(config: ConditionerConfig, packets: u64) -> (Vec<u16>, ConditionerStats) {
        let mut c = NetConditioner::new(config);
        let mut arrivals = Vec::new();
        let mut take = |out: Vec<Datagram>| {
            for (bytes, _) in out {
                arrivals.push(u16::from_le_bytes([bytes[0], bytes[1]]));
            }
        };
        for i in 0..packets {
            take(c.process((i as u16).to_le_bytes().to_vec(), dest(), i));
            take(c.release_due(i));
        }
        take(c.release_due(u64::MAX));
        (arrivals, c.stats())
    }

    #[test]
    fn disabled_is_pass_through() {
        let mut c = NetConditioner::new(ConditionerConfig::default());
        let out = c.process(vec![1, 2, 3], dest(), 0);
        assert_eq!(out, vec![(vec![1, 2, 3], dest())]);
        assert!(c.release_due(u64::MAX).is_empty());
        assert_eq!(c.stats().passed, 1);
    }

    #[test]
    fn same_seed_same_decisions_other_seed_differs() {
        let base = ConditionerConfig::from_profile("wan_stress").expect("profile");
        let a = run(
            ConditionerConfig {
                seed: 77,
                ..base.clone()
            },
            5000,
        );
        let b = run(
            ConditionerConfig {
                seed: 77,
                ..base.clone()
            },
            5000,
        );
        let c = run(ConditionerConfig { seed: 78, ..base }, 5000);
        assert_eq!(a, b);
        assert_ne!(a.0, c.0);
    }

    #[test]
    fn loss_rate_converges() {
        let config = ConditionerConfig {
            enabled: true,
            seed: 3,
            loss: 0.1,
            ..ConditionerConfig::default()
        };
        let (_, stats) = run(config, 20_000);
        let rate = stats.dropped as f64 / 20_000.0;
        assert!((0.09..0.11).contains(&rate), "loss rate {rate}");
    }

    #[test]
    fn delay_holds_until_due_and_jitter_stays_in_range() {
        let mut c = NetConditioner::new(ConditionerConfig {
            enabled: true,
            delay_ms: 100,
            jitter_ms: 20,
            ..ConditionerConfig::default()
        });
        for i in 0..50u8 {
            assert!(c.process(vec![i], dest(), 1000).is_empty());
        }
        assert!(c.release_due(1099).is_empty(), "nothing before the base delay");
        let out = c.release_due(1120);
        assert_eq!(out.len(), 50, "everything by delay + jitter");
        assert_eq!(c.pending(), 0);

        let wan = ConditionerConfig::from_profile("wan").expect("wan");
        let mut wan_c = NetConditioner::new(ConditionerConfig {
            seed: 4,
            ..wan.clone()
        });
        let t0 = 20_000u64;
        for i in 0..40u8 {
            let _ = wan_c.process(vec![i], dest(), t0);
        }
        assert!(
            wan_c.release_due(t0 + wan.delay_ms - 1).is_empty(),
            "a wan hold is never shorter than delay_ms"
        );
    }

    #[test]
    fn reorder_releases_later_datagrams_first() {
        let mut c = NetConditioner::new(ConditionerConfig {
            enabled: true,
            seed: 5,
            reorder: 0.5,
            reorder_max_ms: 50,
            ..ConditionerConfig::default()
        });
        let mut arrivals = Vec::new();
        for t in 0..200u64 {
            for (bytes, _) in c.process(vec![t as u8], dest(), t) {
                arrivals.push(bytes[0]);
            }
            for (bytes, _) in c.release_due(t) {
                arrivals.push(bytes[0]);
            }
        }
        for (bytes, _) in c.release_due(10_000) {
            arrivals.push(bytes[0]);
        }
        assert_eq!(arrivals.len(), 200, "reorder never loses a datagram");
        assert!(
            arrivals.windows(2).any(|w| w[1] < w[0]),
            "some datagram arrived out of order"
        );
    }

    #[test]
    fn burst_drops_everything_inside_its_window() {
        let mut c = NetConditioner::new(ConditionerConfig {
            enabled: true,
            burst_start_ms: 100,
            burst_len_ms: 50,
            burst_loss: 1.0,
            ..ConditionerConfig::default()
        });
        assert_eq!(
            c.process(vec![0], dest(), 1000).len(),
            1,
            "first datagram starts the clock"
        );
        assert_eq!(c.process(vec![1], dest(), 1099).len(), 1);
        assert!(c.process(vec![2], dest(), 1100).is_empty());
        assert!(c.process(vec![3], dest(), 1149).is_empty());
        assert_eq!(c.process(vec![4], dest(), 1150).len(), 1);
    }

    #[test]
    fn profiles_parse_and_sanitize() {
        for name in CONDITIONER_PROFILES {
            assert!(ConditionerConfig::from_profile(name).is_some(), "{name}");
        }
        assert!(ConditionerConfig::from_profile("nope").is_none());
        let wild = ConditionerConfig {
            loss: 7.0,
            duplicate: f64::NAN,
            ..ConditionerConfig::default()
        }
        .sanitized();
        assert_eq!(wild.loss, 1.0);
        assert_eq!(wild.duplicate, 0.0);
        // Huge times are clamped instead of overflowing `jitter_ms + 1` or the burst end.
        let mut c = NetConditioner::new(ConditionerConfig {
            enabled: true,
            jitter_ms: u64::MAX,
            reorder: 1.0,
            reorder_max_ms: u64::MAX,
            burst_start_ms: u64::MAX,
            burst_len_ms: u64::MAX,
            ..ConditionerConfig::default()
        });
        assert_eq!(c.config().jitter_ms, MAX_CONDITIONER_MS);
        for t in 0..100 {
            let _ = c.process(vec![1], dest(), t);
        }
        assert!(c.release_due(u64::MAX).len() <= 100);
    }
}
