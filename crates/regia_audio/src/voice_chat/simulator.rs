// SPDX-License-Identifier: MPL-2.0
//! Seeded packet-loss simulator (`docs/specs/59-voice-chat.md` § 7).
//!
//! Independent or Gilbert-Elliott burst loss (E. N. Gilbert, "Capacity of a burst-noise channel",
//! Bell System Technical Journal 1960), reorder, duplication, base latency and jitter, all in
//! voice frames. The same seed and input yield the same output.

use super::VoiceChatTransport;
use super::error::VoiceChatError;
use super::rng::Pcg32;
use serde::{Deserialize, Serialize};

/// Settings of the simulated network.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimulatorSettings {
    /// Target loss in percent, 0..=95.
    pub loss_pct: f32,
    /// Mean burst length in packets; 1 or less means independent loss.
    pub burst_len: f32,
    /// Percent of packets delayed by two extra frames so they arrive out of order.
    pub reorder_pct: f32,
    /// Percent of packets delivered twice.
    pub dup_pct: f32,
    /// Base latency in frames.
    pub latency_frames: u32,
    /// Extra uniform delay in `0..=jitter_frames` frames.
    pub jitter_frames: u32,
    /// Random seed.
    pub seed: u64,
}

impl Default for SimulatorSettings {
    fn default() -> Self {
        Self {
            loss_pct: 0.0,
            burst_len: 1.0,
            reorder_pct: 0.0,
            dup_pct: 0.0,
            latency_frames: 0,
            jitter_frames: 0,
            seed: 1,
        }
    }
}

impl SimulatorSettings {
    /// Checks the ranges.
    ///
    /// # Errors
    /// [`VoiceChatError::InvalidParams`] naming the field.
    pub fn validate(&self) -> Result<(), VoiceChatError> {
        let pct = |name: &str, v: f32, max: f32| {
            if v.is_finite() && (0.0..=max).contains(&v) {
                Ok(())
            } else {
                Err(VoiceChatError::InvalidParams(format!(
                    "{name} must be in 0..={max}"
                )))
            }
        };
        pct("loss_pct", self.loss_pct, 95.0)?;
        pct("reorder_pct", self.reorder_pct, 100.0)?;
        pct("dup_pct", self.dup_pct, 100.0)?;
        if !self.burst_len.is_finite() || self.burst_len < 0.0 {
            return Err(VoiceChatError::InvalidParams(
                "burst_len must be 0 or more".into(),
            ));
        }
        Ok(())
    }
}

/// Counters of the simulator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimulatorStats {
    /// Packets sent in.
    pub sent: u64,
    /// Packets dropped.
    pub lost: u64,
    /// Extra copies created.
    pub duplicated: u64,
    /// Packets delayed to arrive out of order.
    pub reordered: u64,
    /// Packets delivered.
    pub delivered: u64,
}

struct InFlight {
    due: u64,
    order: u64,
    bytes: Vec<u8>,
}

/// The packet-loss simulator. Its clock is the caller's frame counter via [`advance`](Self::advance).
pub struct VoiceChatSimulator {
    settings: SimulatorSettings,
    rng: Pcg32,
    now: u64,
    order: u64,
    bad: bool,
    in_flight: Vec<InFlight>,
    /// Packet buffers returned by [`Self::poll_packet_into`].
    spare: Vec<Vec<u8>>,
    stats: SimulatorStats,
}

impl VoiceChatSimulator {
    /// A simulator with the given settings.
    ///
    /// # Errors
    /// Whatever [`SimulatorSettings::validate`] reports.
    pub fn new(settings: SimulatorSettings) -> Result<Self, VoiceChatError> {
        settings.validate()?;
        Ok(Self {
            rng: Pcg32::new(settings.seed),
            settings,
            now: 0,
            order: 0,
            bad: false,
            in_flight: Vec::new(),
            spare: Vec::new(),
            stats: SimulatorStats::default(),
        })
    }

    /// Replaces the settings and restarts the random stream; packets in flight stay.
    ///
    /// # Errors
    /// Whatever [`SimulatorSettings::validate`] reports; the old settings stay then.
    pub fn apply(&mut self, settings: SimulatorSettings) -> Result<(), VoiceChatError> {
        settings.validate()?;
        self.rng = Pcg32::new(settings.seed);
        self.settings = settings;
        self.bad = false;
        Ok(())
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> SimulatorStats {
        self.stats
    }

    /// Moves the clock on by one frame.
    pub fn advance(&mut self) {
        self.now += 1;
    }

    fn lose(&mut self) -> bool {
        let p = f64::from(self.settings.loss_pct) / 100.0;
        if p <= 0.0 {
            return false;
        }
        if self.settings.burst_len <= 1.0 {
            return f64::from(self.rng.next_f32()) < p;
        }
        // Gilbert-Elliott: Bad drops everything; mean burst = 1 / p_bad_to_good, and the
        // stationary loss p = p_gb / (p_gb + p_bg).
        let p_bg = 1.0 / f64::from(self.settings.burst_len);
        let p_gb = (p * p_bg / (1.0 - p)).min(1.0);
        let flip = f64::from(self.rng.next_f32());
        self.bad = if self.bad { flip >= p_bg } else { flip < p_gb };
        self.bad
    }

    /// Sends a packet through the simulated network.
    pub fn send_packet(&mut self, packet: &[u8]) {
        self.stats.sent += 1;
        if self.lose() {
            self.stats.lost += 1;
            return;
        }
        let jitter = self.rng.next_up_to(self.settings.jitter_frames);
        let mut due = self.now + u64::from(self.settings.latency_frames) + u64::from(jitter);
        if f64::from(self.rng.next_f32()) * 100.0 < f64::from(self.settings.reorder_pct) {
            due += 2;
            self.stats.reordered += 1;
        }
        let dup = f64::from(self.rng.next_f32()) * 100.0 < f64::from(self.settings.dup_pct);
        self.enqueue(due, packet);
        if dup {
            self.stats.duplicated += 1;
            self.enqueue(due + 1, packet);
        }
    }

    fn take_bytes(&mut self, packet: &[u8]) -> Vec<u8> {
        let mut bytes = self.spare.pop().unwrap_or_default();
        bytes.clear();
        bytes.extend_from_slice(packet);
        bytes
    }

    fn enqueue(&mut self, due: u64, packet: &[u8]) {
        self.order += 1;
        let bytes = self.take_bytes(packet);
        self.in_flight.push(InFlight {
            due,
            order: self.order,
            bytes,
        });
    }

    /// Copies the next packet due at the current time into `out` and recycles its buffer.
    ///
    /// Returns whether a packet was due. `out` keeps its capacity across calls.
    pub fn poll_packet_into(&mut self, out: &mut Vec<u8>) -> bool {
        let Some((idx, _)) = self
            .in_flight
            .iter()
            .enumerate()
            .filter(|(_, packet)| packet.due <= self.now)
            .min_by_key(|(_, packet)| (packet.due, packet.order))
        else {
            return false;
        };
        let packet = self.in_flight.swap_remove(idx);
        out.clear();
        out.extend_from_slice(&packet.bytes);
        if self.spare.len() < 64 {
            self.spare.push(packet.bytes);
        }
        self.stats.delivered += 1;
        true
    }

    /// Removes and returns the next packet due at the current time, in (due, send) order.
    pub fn poll_packet(&mut self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        self.poll_packet_into(&mut out).then_some(out)
    }
}

impl VoiceChatTransport for VoiceChatSimulator {
    fn send(&mut self, packet: &[u8]) {
        self.send_packet(packet);
    }

    fn poll(&mut self) -> Option<Vec<u8>> {
        self.poll_packet()
    }
}
