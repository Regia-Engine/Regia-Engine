// SPDX-License-Identifier: MPL-2.0
//! Snapshot interpolation for proxies (spec 74).
//!
//! Proxies change only when state arrives, at the send rate, with jitter and loss. A client shows
//! them a short, adaptive delay in the past: each proxy keeps a [`SnapshotBuffer`] of received
//! states keyed by server tick, and every frame the [`InterpClock`] gives the fractional
//! interpolation tick at which the buffer is sampled. When data runs out the proxy extrapolates
//! briefly, then holds; when data resumes, the difference becomes a decaying visual offset.
//!
//! | Piece | What |
//! |---|---|
//! | [`InterpClock`] | `server_tick_estimate`, arrival jitter, target delay, clock rate, resyncs, `interp_tick` |
//! | [`SnapshotBuffer`] | per-proxy states, bracketing, snap and gap rules, Linear / Hermite, extrapolation, recovery |
//! | [`NetInterpolation`] | the client resource: opt-in switch, clock, counters, the agent data |
//! | [`net_interpolate`] | the per-frame pass (span `net_interp`): transforms, interpolated and step components, spawn and despawn timing |
//! | [`NetViewTick`] / [`ViewTickWire`] | the view tick reported to the server for lag compensation (§ 8) |
//!
//! The core is pure: it never reads the wall clock. Hosts pass the local time in milliseconds
//! ([`NetInterpolation::now_ms`] before applying packets, `now_ms` to [`net_interpolate`]); the
//! [`NetInterpolationPlugin`] takes it from `RealTime`, tests from a simulated clock. Given the
//! same arrival log the shown poses are identical (§ 11).
//!
//! Interpolation is opt-in per client process: without a [`NetInterpolation`] resource, received
//! transforms and components are written on arrival (the spec 71 behavior).

use std::any::TypeId;
use std::collections::VecDeque;

use glam::{Quat, Vec3};
use regia_app::{App, Plugin, PostUpdate, PreUpdate, RealTime};
use regia_ecs::prelude::*;
use regia_scene::components::Visibility;
use regia_scene::transform::Transform;
use serde::{Deserialize, Serialize};

use crate::bits::{BitReader, BitWriter, DecodeError};
use crate::codec::ReplicateCodec;
use crate::model::{NetId, NetMode, NetOwnedLocally, NetTick};
use crate::plugin::NetSet;
use crate::replication::{
    NetOutOfScope, NetPredicted, NetTransformMode, NetVelocity, ReplicationRegistry, TransformReplication,
};
use crate::runtime::NetInterpTick;
use crate::settings::{InterpolationSettings, NetSettings};

// ---- profile ------------------------------------------------------------------------------------

pub use crate::profile::InterpolationMode;

/// The interpolation fields of a `.netprofile.ron` (spec 74 § 10). Serde defaults, so profiles
/// written without them load.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InterpProfile {
    /// Linear, Hermite or None.
    pub interpolation: InterpolationMode,
    /// A jump between two states longer than this is not blended, in meters.
    pub snap_distance_m: f32,
    /// How long a moving proxy extrapolates past its newest state, in milliseconds (0 disables).
    pub max_extrapolate_ms: f32,
    /// Show a spawned proxy at once instead of when its spawn tick is shown.
    pub appear_immediately: bool,
}

impl Default for InterpProfile {
    fn default() -> Self {
        Self {
            interpolation: InterpolationMode::Linear,
            snap_distance_m: 5.0,
            max_extrapolate_ms: 100.0,
            appear_immediately: false,
        }
    }
}

/// Client: the interpolation profile of one proxy. Proxies without it use
/// [`NetInterpolation::default_profile`].
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct NetInterpProfile(pub InterpProfile);

// ---- clock --------------------------------------------------------------------------------------

/// Arrival samples kept for the estimate and the jitter (spec 74 § 4).
pub const ARRIVAL_WINDOW: usize = 64;
/// Arrivals within which a second spike makes the first one count (spec 120 § 9.1's 8-sample
/// window; ADR-8312): one delayed state packet never moves the clock, two within this many
/// arrivals are jitter.
pub const OFFSET_FILTER_SAMPLES: usize = 8;
/// An arrival this many times the jitter spread (and at least this many send intervals) later
/// than the clock offset is a spike (the popcorn spike suppressor of RFC 5905 § 10): kept out of
/// the jitter unless a second one follows within [`OFFSET_FILTER_SAMPLES`] arrivals.
const SPIKE_GATE: f64 = 3.0;
/// Proportional range of the clock-rate controller: a delay error this large (ms) uses the full
/// raise or lower rate.
const RATE_RAMP_MS: f64 = 50.0;
/// Late samples must persist this long (local ms, as a fraction of the resync error) before the
/// estimate follows them; a burst that arrives at one instant never moves it.
const LATE_PERSIST_FRACTION: f64 = 0.5;
/// Late samples needed before the estimate follows them.
const LATE_PERSIST_SAMPLES: usize = 4;
/// How long a measured loss gap keeps raising the delay, in milliseconds of server time
/// (spec 120 § 9.7). Bursts on a bad link come every few seconds; a gap older than this is
/// forgotten and the delay lowers again (slowly, at `lower_rate`).
pub const LOSS_MEMORY_MS: f64 = 10_000.0;
/// Loss gaps remembered at most (the oldest go first).
const LOSS_GAPS_KEPT: usize = 64;
/// A spacing between two received state frames over this many send intervals is a gap (lost
/// or not-yet-arrived frames); shorter ones are send-time rounding.
const GAP_SEND_INTERVALS: f64 = 1.5;
/// A gap counts as loss once frames this many send intervals past it have arrived: a frame that
/// was only overtaken (reordered) fills its gap before then, so jitter that reorders does not
/// move the delay.
const GAP_CONFIRM_SEND_INTERVALS: f64 = 2.0;

/// The client's interpolation timeline (spec 74 § 4): estimates the server tick at any local
/// instant from state arrivals, tracks arrival jitter, and runs `interp_tick` a target delay
/// behind it by changing its speed, never by jumping (except on a resync).
#[derive(Debug, Clone)]
pub struct InterpClock {
    settings: InterpolationSettings,
    tick_hz: f64,
    send_interval_ticks: f64,
    samples: VecDeque<f64>,
    /// A held spike: `(arrival count, offset)`.
    spike: Option<(u64, f64)>,
    /// Arrivals taken, for the spike's age.
    arrivals: u64,
    /// Local time of the last arrival taken, for the offset's slew.
    last_arrival_ms: Option<f64>,
    offset_ms: f64,
    jitter_ms: f64,
    late_run: Option<(f64, Vec<f64>)>,
    interp_tick: Option<f64>,
    last_now: Option<f64>,
    rate: f64,
    holding: bool,
    override_ms: Option<f64>,
    resyncs: u64,
    force_resync: bool,
    newest_tick: Option<u64>,
    /// Measured gaps `(tick before, tick after)` between received state frames (spec 120 § 9.7).
    gaps: VecDeque<(u64, u64)>,
}

impl InterpClock {
    /// A clock for a server ticking at `tick_hz` that sends state every `send_interval_ticks`.
    pub fn new(settings: InterpolationSettings, tick_hz: f64, send_interval_ticks: f64) -> Self {
        Self {
            settings,
            tick_hz: if tick_hz > 0.0 { tick_hz } else { 60.0 },
            send_interval_ticks: send_interval_ticks.max(1.0),
            samples: VecDeque::with_capacity(ARRIVAL_WINDOW),
            spike: None,
            arrivals: 0,
            last_arrival_ms: None,
            offset_ms: 0.0,
            jitter_ms: 0.0,
            late_run: None,
            interp_tick: None,
            last_now: None,
            rate: 1.0,
            holding: false,
            override_ms: None,
            resyncs: 0,
            force_resync: false,
            newest_tick: None,
            gaps: VecDeque::new(),
        }
    }

    /// Milliseconds per server tick.
    pub fn ms_per_tick(&self) -> f64 {
        1000.0 / self.tick_hz
    }

    /// Server ticks between state packets.
    pub fn send_interval_ticks(&self) -> f64 {
        self.send_interval_ticks
    }

    /// Adopts the server's simulation rate and send cadence (from `Welcome`). The arrival offset
    /// is `arrival - tick * ms_per_tick`, so a clock built for another rate drifts every tick.
    /// Restarts the arrival estimate: it was measured at the old rate.
    pub fn set_server_rate(&mut self, tick_hz: f64, send_interval_ticks: f64) {
        if tick_hz.is_finite() && tick_hz > 0.0 {
            self.tick_hz = tick_hz;
        }
        self.send_interval_ticks = send_interval_ticks.max(1.0);
        self.restart();
    }

    /// Forgets the arrival estimate and the playback position (a new server's tick numbering).
    pub fn restart(&mut self) {
        self.clear_samples();
        self.offset_ms = 0.0;
        self.jitter_ms = 0.0;
        self.late_run = None;
        self.interp_tick = None;
        self.last_now = None;
        self.rate = 1.0;
        self.holding = false;
        self.newest_tick = None;
        self.gaps.clear();
    }

    /// Sets the observed send interval in ticks (at least 1).
    pub fn set_send_interval_ticks(&mut self, ticks: f64) {
        self.send_interval_ticks = ticks.max(1.0);
    }

    /// The settings in use.
    pub fn settings(&self) -> &InterpolationSettings {
        &self.settings
    }

    /// Records a state packet for `server_tick` that arrived at local time `arrival_ms`.
    pub fn on_state(&mut self, server_tick: u64, arrival_ms: f64) {
        self.note_gap(server_tick);
        self.newest_tick = Some(self.newest_tick.map_or(server_tick, |n| n.max(server_tick)));
        let o = arrival_ms - server_tick as f64 * self.ms_per_tick();
        if self.samples.is_empty() {
            self.push_sample(o, arrival_ms);
            return;
        }
        let limit = self.settings.resync_error_ms;
        if o > self.offset_ms + limit {
            // Much later than the estimate expects: a burst after a stall (ignored), or the
            // server's clock fell behind (followed once it persists).
            let run = self.late_run.get_or_insert((arrival_ms, Vec::new()));
            run.1.push(o);
            let persisted =
                arrival_ms - run.0 >= limit * LATE_PERSIST_FRACTION && run.1.len() >= LATE_PERSIST_SAMPLES;
            if persisted {
                let run = self.late_run.take().map(|r| r.1).unwrap_or_default();
                self.clear_samples();
                for s in run {
                    self.push_sample(s, arrival_ms);
                }
            }
            return;
        }
        self.late_run = None;
        if o < self.offset_ms - limit {
            // Much earlier: the server is ahead of the estimate; earlier evidence is reliable.
            self.clear_samples();
        }
        self.push_sample(o, arrival_ms);
    }

    /// Records a state frame for `server_tick` that arrived after a newer one (reordered): it
    /// fills a loss gap (spec 120 § 9.7) but is no arrival-time sample. The client's state
    /// channel calls this for frames older than its newest; [`Self::on_state`] does the same for
    /// an old tick.
    pub fn on_late_state(&mut self, server_tick: u64) {
        if self.newest_tick.is_some_and(|n| server_tick < n) {
            self.note_gap(server_tick);
        }
    }

    /// Loss-run measurement (spec 120 § 9.7): a frame that skips past the newest one by more than
    /// 1.5 send intervals opens a gap; a late (reordered) frame inside a gap splits it, so
    /// reordering is not counted as loss. Gaps older than [`LOSS_MEMORY_MS`] are forgotten.
    fn note_gap(&mut self, tick: u64) {
        let Some(newest) = self.newest_tick else {
            return;
        };
        let min_gap = self.send_interval_ticks * GAP_SEND_INTERVALS;
        if tick > newest {
            if (tick - newest) as f64 > min_gap {
                self.gaps.push_back((newest, tick));
                while self.gaps.len() > LOSS_GAPS_KEPT {
                    self.gaps.pop_front();
                }
            }
        } else if let Some(i) = self.gaps.iter().position(|&(a, b)| a < tick && tick < b) {
            let (a, b) = self.gaps.remove(i).unwrap_or((tick, tick));
            for (x, y) in [(a, tick), (tick, b)] {
                if (y - x) as f64 > min_gap {
                    self.gaps.push_back((x, y));
                }
            }
        }
        let memory_ticks = (LOSS_MEMORY_MS / self.ms_per_tick()) as u64;
        let horizon = newest.max(tick).saturating_sub(memory_ticks);
        self.gaps.retain(|&(_, b)| b >= horizon);
    }

    /// The loss term of the target delay (spec 120 § 9.7), in milliseconds: how much the longest
    /// gap between two received state frames over the last [`LOSS_MEMORY_MS`] exceeds the
    /// `delay_send_intervals` send intervals the delay already holds (which cover one lost frame
    /// at the default 2): the measured burst-loss run. A gap counts once frames two send
    /// intervals past it have arrived without its missing frames (reordering fills a gap before).
    /// A gap no allowed delay could cover (longer than `max_delay_ms`) is an outage, handled by
    /// extrapolation and resync, and is not counted.
    pub fn loss_gap_ms(&self) -> f64 {
        let mspt = self.ms_per_tick();
        let send = self.send_interval_ticks;
        let max = self.settings.max_delay_ms;
        let Some(newest) = self.newest_tick else {
            return 0.0;
        };
        let confirm = send * GAP_CONFIRM_SEND_INTERVALS;
        let covered = send * self.settings.delay_send_intervals.max(0.0);
        self.gaps
            .iter()
            .filter(|&&(_, b)| (newest.saturating_sub(b)) as f64 >= confirm)
            .map(|&(a, b)| ((b - a) as f64 - covered).max(0.0) * mspt)
            .filter(|ms| *ms <= max)
            .fold(0.0, f64::max)
    }

    /// Forgets every arrival offset (the window, the offset filter and a held spike).
    fn clear_samples(&mut self) {
        self.samples.clear();
        self.spike = None;
        self.last_arrival_ms = None;
    }

    /// Takes one arrival offset (spec 74 § 4 with spec 120 § 9.1, ADR-8312). An arrival more than
    /// `SPIKE_GATE` jitters (and send intervals) above the offset is held out until a second one
    /// follows within [`OFFSET_FILTER_SAMPLES`] arrivals (the popcorn spike suppressor of RFC 5905
    /// § 10): one late packet changes neither the estimate nor the jitter, while real jitter
    /// counts from its second sample. The offset and the jitter are the mean and the mean
    /// absolute deviation of the last [`ARRIVAL_WINDOW`] offsets taken (spec 74); a later offset
    /// is slewed in at 5 % of the local time since the last arrival, so the estimate never steps
    /// backward, and an earlier one is taken at once.
    fn push_sample(&mut self, o: f64, arrival_ms: f64) {
        self.arrivals += 1;
        let interval_ms = self.send_interval_ticks * self.ms_per_tick();
        let gate = SPIKE_GATE * (2.0 * self.jitter_ms).max(interval_ms);
        if self.samples.len() >= OFFSET_FILTER_SAMPLES && o - self.offset_ms > gate {
            match self.spike.take() {
                Some((at, held)) if self.arrivals - at <= OFFSET_FILTER_SAMPLES as u64 => {
                    self.samples.push_back(held);
                }
                _ => {
                    self.spike = Some((self.arrivals, o));
                    return;
                }
            }
        }
        self.samples.push_back(o);
        while self.samples.len() > ARRIVAL_WINDOW {
            self.samples.pop_front();
        }
        let n = self.samples.len() as f64;
        let mean = self.samples.iter().sum::<f64>() / n;
        self.jitter_ms = self.samples.iter().map(|s| (s - mean).abs()).sum::<f64>() / n;
        // A later offset moves the estimate back: slew toward it at most `feedback::CLOCK_SLEW`
        // of the local time since the last arrival (spec 120 § 9.1).
        self.offset_ms = match self.last_arrival_ms {
            Some(last) if mean > self.offset_ms => {
                let room = crate::feedback::CLOCK_SLEW * (arrival_ms - last).max(0.0);
                mean.min(self.offset_ms + room)
            }
            _ => mean,
        };
        self.last_arrival_ms = Some(self.last_arrival_ms.map_or(arrival_ms, |l| l.max(arrival_ms)));
    }

    /// The estimated server tick at local time `now_ms` (`None` before the first state).
    pub fn server_tick_estimate(&self, now_ms: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        Some((now_ms - self.offset_ms) / self.ms_per_tick())
    }

    /// Arrival jitter (mean absolute deviation over the last 64 arrivals, a lone spike held out;
    /// ADR-8312), in milliseconds.
    pub fn jitter_ms(&self) -> f64 {
        self.jitter_ms
    }

    /// The delay the clock steers toward, in milliseconds: `delay_send_intervals` send intervals
    /// plus twice the jitter plus the loss term ([`Self::loss_gap_ms`], spec 120 § 9.7), clamped
    /// to `min_delay_ms..=max_delay_ms`.
    pub fn target_delay_ms(&self) -> f64 {
        if let Some(ms) = self.override_ms {
            return ms;
        }
        let base = self.send_interval_ticks * self.ms_per_tick() * self.settings.delay_send_intervals;
        let min = self.settings.min_delay_ms;
        let max = self.settings.max_delay_ms.max(min);
        (base + 2.0 * self.jitter_ms + self.loss_gap_ms()).clamp(min, max)
    }

    /// The current delay (`server_tick_estimate − interp_tick`) at `now_ms`, in milliseconds.
    pub fn delay_ms(&self, now_ms: f64) -> Option<f64> {
        let est = self.server_tick_estimate(now_ms)?;
        let it = self.interp_tick?;
        Some((est - it) * self.ms_per_tick())
    }

    /// The interpolation tick of the last [`Self::advance`].
    pub fn interp_tick(&self) -> Option<f64> {
        self.interp_tick
    }

    /// The clock speed of the last advance (1.0 = real time).
    pub fn clock_rate(&self) -> f64 {
        self.rate
    }

    /// Resyncs so far.
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// The newest server tick received.
    pub fn newest_tick(&self) -> Option<u64> {
        self.newest_tick
    }

    /// Fixed delay for `net.set_interp_delay` (`None` = adaptive).
    pub fn set_delay_override(&mut self, ms: Option<f64>) {
        self.override_ms = ms;
    }

    /// The fixed delay, if one is set.
    pub fn delay_override(&self) -> Option<f64> {
        self.override_ms
    }

    /// Forces a resync on the next advance (an Android `resumed`: the app was stopped).
    pub fn request_resync(&mut self) {
        self.force_resync = true;
    }

    /// Advances to local time `now_ms` and returns the interpolation tick (`None` before the
    /// first state). Monotonic: never goes backward, also across a resync.
    pub fn advance(&mut self, now_ms: f64) -> Option<f64> {
        let Some(est) = self.server_tick_estimate(now_ms) else {
            self.last_now = Some(now_ms);
            return None;
        };
        let mspt = self.ms_per_tick();
        let target_ms = self.target_delay_ms();
        let want = est - target_ms / mspt;
        let Some(cur) = self.interp_tick else {
            self.interp_tick = Some(want);
            self.last_now = Some(now_ms);
            self.rate = 1.0;
            return self.interp_tick;
        };
        let dt = (now_ms - self.last_now.unwrap_or(now_ms)).max(0.0);
        self.last_now = Some(now_ms);
        let limit = self.settings.resync_error_ms;
        if self.holding {
            // Waiting for the target to catch up with a held clock.
            if want >= cur {
                self.holding = false;
                self.rate = 1.0;
            } else {
                self.rate = 0.0;
                return Some(cur);
            }
        }
        let mut next = cur + dt / mspt * self.rate;
        let error_ms = target_ms - (est - next) * mspt;
        if self.force_resync || dt > limit || error_ms.abs() > limit {
            self.force_resync = false;
            self.resyncs += 1;
            tracing::info!(target: "net", error_ms, frame_gap_ms = dt, "net.interp_resync");
            if want > cur {
                next = want;
                self.rate = 1.0;
            } else {
                next = cur;
                self.holding = true;
                self.rate = 0.0;
            }
        } else {
            let f = (error_ms / RATE_RAMP_MS).clamp(-1.0, 1.0);
            // Delay must grow (f > 0): run slower; delay must shrink: run faster, gently.
            self.rate = if f > 0.0 {
                1.0 - self.settings.raise_rate * f
            } else {
                1.0 - self.settings.lower_rate * f
            };
        }
        let next = next.max(cur);
        self.interp_tick = Some(next);
        Some(next)
    }
}

// ---- snapshot buffer ----------------------------------------------------------------------------

/// One received transform state of a proxy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProxyState {
    /// The server tick of the state.
    pub tick: u64,
    /// Position.
    pub translation: Vec3,
    /// Rotation.
    pub rotation: Quat,
    /// Scale.
    pub scale: Vec3,
    /// Replicated linear velocity, units per second.
    pub velocity: Option<Vec3>,
    /// The state carries the teleport bit.
    pub teleport: bool,
    /// The state's record carries the steady bit (spec 120 § 8.2, ADR-8522): the server keeps the
    /// copy within its extrapolation gate, so past this state the proxy extrapolates without the
    /// `max_extrapolate_ms` limit and is never `Starved`.
    pub steady: bool,
}

/// How a proxy is being shown (agent `net.proxy_timeline` `state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyTimelineState {
    /// Blending between two states that bracket the interpolation tick.
    Interpolating,
    /// Past the newest state, extrapolating with its velocity.
    Extrapolating,
    /// Past the newest state and the extrapolation limit: held, `NetStarved`.
    Starved,
    /// Showing one state (stopped, before the first state, or `interpolation: None`).
    #[default]
    Holding,
}

impl ProxyTimelineState {
    /// The name used in agent replies.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interpolating => "interpolating",
            Self::Extrapolating => "extrapolating",
            Self::Starved => "starved",
            Self::Holding => "holding",
        }
    }
}

/// A shown pose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    /// Position.
    pub translation: Vec3,
    /// Rotation.
    pub rotation: Quat,
    /// Scale.
    pub scale: Vec3,
    /// Interpolated velocity (animation input), when the states carry velocities.
    pub velocity: Option<Vec3>,
}

/// The result of sampling a buffer at an interpolation tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// The pose to show (recovery offset included).
    pub pose: Pose,
    /// How it was produced.
    pub state: ProxyTimelineState,
    /// The bracketing pair's ticks, when interpolating.
    pub bracket: Option<(u64, u64)>,
    /// Blend parameter within the bracket.
    pub t: f32,
    /// This frame snapped (a teleport, a jump over `snap_distance_m`, or an offset over it).
    pub snapped: bool,
    /// This frame started a recovery from extrapolation.
    pub recovered: bool,
}

/// Inputs of [`SnapshotBuffer::sample`] that come from the clock and settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampleParams {
    /// Server ticks per second.
    pub tick_hz: f64,
    /// Server ticks between state packets.
    pub send_interval_ticks: f64,
    /// Half-life of the recovery offset, in milliseconds.
    pub recover_half_life_ms: f64,
    /// The newest server tick received in any state frame (spec 120 § 9.7). A moving
    /// velocity-less proxy is extrapolated only while frames newer than its newest state have
    /// arrived without it (the budget defers it) and its next state is not overdue (else it
    /// stopped). `None` (unknown) never extrapolates it.
    pub newest_tick: Option<u64>,
}

impl Default for SampleParams {
    fn default() -> Self {
        Self {
            tick_hz: 60.0,
            send_interval_ticks: 2.0,
            recover_half_life_ms: 60.0,
            newest_tick: None,
        }
    }
}

/// The longest own update interval (spacing between a proxy's last two states) that counts as a
/// budget deferral, in milliseconds (spec 120 § 9.7). A longer spacing is a rest followed by a
/// move: no velocity is inferred across it and the gap rule uses the send interval.
pub const MAX_DEFERRAL_MS: f64 = 500.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Basis {
    Before,
    Pair(u64, u64),
    From(ProxyState),
    /// Extrapolating a velocity-less state with the velocity inferred from the state before it
    /// (stored in `velocity`), up to `cap_ms` past it.
    Inferred(ProxyState, f64),
}

/// What a velocity-less proxy's previous state says about its newest one (spec 120 § 9.7).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Spacing {
    /// Ticks between the two states: the proxy's own update interval.
    ticks: f64,
    /// The velocity between them, units per second (zero when they are equal: at rest).
    velocity: Vec3,
}

#[derive(Debug, Clone, Copy)]
struct LastFrame {
    state: ProxyTimelineState,
    basis: Basis,
    left: Option<u64>,
}

struct Raw {
    pose: Pose,
    state: ProxyTimelineState,
    bracket: Option<(u64, u64)>,
    t: f32,
    basis: Basis,
    left: Option<u64>,
    snap_pair: Option<u64>,
}

/// Per proxy: the received transform states keyed by server tick (a ring of `buffer_states`),
/// plus the recovery offset (spec 74 § 5, § 7).
#[derive(Debug, Clone)]
pub struct SnapshotBuffer {
    states: VecDeque<ProxyState>,
    capacity: usize,
    offset: Vec3,
    last: Option<LastFrame>,
    snap_target: Option<u64>,
    /// The newest state dropped from the front (the one before the left bracket), kept for the
    /// proxy's own update interval and inferred velocity.
    dropped: Option<ProxyState>,
}

impl Default for SnapshotBuffer {
    fn default() -> Self {
        Self::new(32)
    }
}

fn nlerp(a: Quat, b: Quat, t: f32) -> Quat {
    // Scalar dot and normalization (spec 71 § 12): this rotation is written into every proxy's
    // `Transform`, and glam's SIMD dot4 pairs components differently on SSE2 and NEON, so two
    // clients would show different proxy bits for identical snapshots.
    let b = if crate::scalar::quat_dot(a, b) < 0.0 {
        -b
    } else {
        b
    };
    let q = a * (1.0 - t) + b * t;
    // Degenerate input: keep the unnormalized value (see `lagcomp::geom::blend`).
    crate::scalar::normalize4(q.to_array()).map_or(q, Quat::from_array)
}

fn pose_of(s: &ProxyState) -> Pose {
    Pose {
        translation: s.translation,
        rotation: s.rotation,
        scale: s.scale,
        velocity: s.velocity,
    }
}

fn moving(v: Option<Vec3>) -> Option<Vec3> {
    v.filter(|v| v.length_squared() > 0.0)
}

impl SnapshotBuffer {
    /// Moves every buffered position by `delta` (a floating-origin rebase, spec 78 § 12.2): the
    /// states, and the state the last frame was shown from. The recovery offset is a difference
    /// and stays.
    pub fn shift_origin(&mut self, delta: Vec3) {
        if !delta.is_finite() {
            return;
        }
        for state in self.states.iter_mut().chain(self.dropped.as_mut()) {
            state.translation += delta;
        }
        if let Some(LastFrame {
            basis: Basis::From(state) | Basis::Inferred(state, _),
            ..
        }) = &mut self.last
        {
            state.translation += delta;
        }
    }

    /// An empty buffer holding up to `capacity` states (at least 2).
    pub fn new(capacity: usize) -> Self {
        Self {
            states: VecDeque::with_capacity(capacity.max(2)),
            capacity: capacity.max(2),
            offset: Vec3::ZERO,
            last: None,
            snap_target: None,
            dropped: None,
        }
    }

    /// Adds a received state, in tick order (a reordered state is inserted; a duplicate tick
    /// replaces). The oldest state goes when the ring is full.
    pub fn push(&mut self, state: ProxyState) {
        match self.states.binary_search_by_key(&state.tick, |s| s.tick) {
            Ok(i) => {
                if let Some(s) = self.states.get_mut(i) {
                    *s = state;
                }
            }
            Err(i) => self.states.insert(i, state),
        }
        while self.states.len() > self.capacity {
            self.states.pop_front();
        }
    }

    /// Drops every state and the recovery offset (a new server's ticks restart from its own
    /// numbering, so nothing buffered from the previous one is comparable).
    pub fn clear(&mut self) {
        self.states.clear();
        self.offset = Vec3::ZERO;
        self.last = None;
        self.snap_target = None;
        self.dropped = None;
    }

    /// The buffered ticks, oldest first.
    pub fn ticks(&self) -> Vec<u64> {
        self.states.iter().map(|s| s.tick).collect()
    }

    /// The buffered states, oldest first.
    pub fn states(&self) -> impl Iterator<Item = &ProxyState> {
        self.states.iter()
    }

    /// The newest buffered state.
    pub fn newest(&self) -> Option<&ProxyState> {
        self.states.back()
    }

    /// Whether nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// The current recovery offset.
    pub fn offset(&self) -> Vec3 {
        self.offset
    }

    /// The state of the last sample.
    pub fn last_state(&self) -> ProxyTimelineState {
        self.last.map(|l| l.state).unwrap_or_default()
    }

    fn extrapolated(s: &ProxyState, interp: f64, profile: &InterpProfile, mspt: f64) -> (Pose, bool) {
        Self::extrapolate_by(s, interp, f64::from(profile.max_extrapolate_ms.max(0.0)), mspt)
    }

    /// `s` moved on with its velocity to `interp`, at most `max_ms` past it; and whether `interp`
    /// is past that limit.
    fn extrapolate_by(s: &ProxyState, interp: f64, max_ms: f64, mspt: f64) -> (Pose, bool) {
        let mut pose = pose_of(s);
        let Some(v) = moving(s.velocity) else {
            return (pose, false);
        };
        let elapsed_ms = ((interp - s.tick as f64) * mspt).max(0.0);
        // A steady record (the extrapolation gate, ADR-8522) is extrapolated without a limit.
        let max_ms = if s.steady { f64::INFINITY } else { max_ms };
        let used = elapsed_ms.min(max_ms);
        pose.translation += v * (used / 1000.0) as f32;
        (pose, elapsed_ms > max_ms)
    }

    /// The proxy's own update interval at `states[i]` and the velocity from the state before it
    /// to it (spec 120 § 9.7): `None` without a known previous state, across a teleport or a
    /// snap jump, or when the spacing is over [`MAX_DEFERRAL_MS`] (a rest, not a deferral).
    fn spacing(&self, i: usize, profile: &InterpProfile, mspt: f64) -> Option<Spacing> {
        let a = self.states.get(i)?;
        let prev = match i.checked_sub(1) {
            Some(j) => self.states.get(j).copied(),
            None => self.dropped.filter(|d| d.tick < a.tick),
        }?;
        let ticks = a.tick.checked_sub(prev.tick)? as f64;
        if ticks <= 0.0
            || ticks * mspt > MAX_DEFERRAL_MS
            || a.teleport
            || prev.translation.distance(a.translation) > profile.snap_distance_m
        {
            return None;
        }
        let seconds = (ticks * mspt / 1000.0) as f32;
        Some(Spacing {
            ticks,
            velocity: (a.translation - prev.translation) / seconds,
        })
    }

    /// The update interval of the newest state (ticks since the state before it), when known
    /// (agent `net.proxy_timeline` `update_interval_ticks`).
    pub fn update_interval_ticks(&self) -> Option<u64> {
        let a = self.states.back()?;
        let prev = match self.states.len().checked_sub(2) {
            Some(j) => self.states.get(j).copied(),
            None => self.dropped,
        }?;
        a.tick.checked_sub(prev.tick).filter(|t| *t > 0)
    }

    fn raw(&self, interp: f64, profile: &InterpProfile, p: &SampleParams) -> Option<Raw> {
        let first = self.states.front()?;
        let mspt = 1000.0 / p.tick_hz.max(1e-6);
        if profile.interpolation == InterpolationMode::None {
            let newest = self.states.back().unwrap_or(first);
            // A steady state moves along its velocity (ADR-8522); any other is held.
            let (pose, state) =
                if newest.steady && moving(newest.velocity).is_some() && interp > newest.tick as f64 {
                    (
                        Self::extrapolated(newest, interp, profile, mspt).0,
                        ProxyTimelineState::Extrapolating,
                    )
                } else {
                    (pose_of(newest), ProxyTimelineState::Holding)
                };
            return Some(Raw {
                pose,
                state,
                bracket: None,
                t: 1.0,
                basis: Basis::From(*newest),
                left: Some(newest.tick),
                snap_pair: None,
            });
        }
        // The newest state at or before the interpolation tick.
        let ia = self.states.iter().rposition(|s| s.tick as f64 <= interp);
        let Some(ia) = ia else {
            return Some(Raw {
                pose: pose_of(first),
                state: ProxyTimelineState::Holding,
                bracket: None,
                t: 0.0,
                basis: Basis::Before,
                left: None,
                snap_pair: None,
            });
        };
        let a = self.states.get(ia)?;
        let send = p.send_interval_ticks.max(1.0);
        let spacing = self.spacing(ia, profile, mspt);
        let Some(b) = self.states.get(ia + 1) else {
            // A velocity-less proxy that was moving (its last two states differ) and that the
            // server's budget defers: extrapolate it with the inferred velocity until its next
            // state is overdue (the stream went on without it: it stopped), for at most its own
            // update interval or `max_extrapolate_ms`, whichever is longer (spec 120 § 9.7).
            // Deferred, not starved: frames newer than its newest state have arrived without it.
            // When the whole stream is late (no newer frame) it is held as before, so a lossy link
            // shows only states the server had.
            if a.velocity.is_none()
                && profile.max_extrapolate_ms > 0.0
                && let Some(sp) = spacing
                && sp.velocity.length_squared() > 0.0
                && p.newest_tick
                    .is_some_and(|n| n > a.tick && (n as f64) < a.tick as f64 + sp.ticks.max(send) + send)
            {
                let cap_ms = (sp.ticks * mspt).max(f64::from(profile.max_extrapolate_ms));
                let s = ProxyState {
                    velocity: Some(sp.velocity),
                    ..*a
                };
                let (mut pose, over) = Self::extrapolate_by(&s, interp, cap_ms, mspt);
                // The velocity is a guess: it is not written as `NetVelocity`.
                pose.velocity = None;
                let state = if over {
                    ProxyTimelineState::Starved
                } else if interp > a.tick as f64 {
                    ProxyTimelineState::Extrapolating
                } else {
                    ProxyTimelineState::Holding
                };
                return Some(Raw {
                    pose,
                    state,
                    bracket: None,
                    t: 0.0,
                    basis: Basis::Inferred(s, cap_ms),
                    left: Some(a.tick),
                    snap_pair: None,
                });
            }
            // Starved: hold a stopped entity, extrapolate a moving one up to the limit.
            let (pose, over) = Self::extrapolated(a, interp, profile, mspt);
            let state = match moving(a.velocity) {
                None => ProxyTimelineState::Holding,
                Some(_) if over || (profile.max_extrapolate_ms <= 0.0 && !a.steady) => {
                    ProxyTimelineState::Starved
                }
                Some(_) if interp > a.tick as f64 => ProxyTimelineState::Extrapolating,
                Some(_) => ProxyTimelineState::Holding,
            };
            return Some(Raw {
                pose,
                state,
                bracket: None,
                t: 0.0,
                basis: Basis::From(*a),
                left: Some(a.tick),
                snap_pair: None,
            });
        };
        let bracket = Some((a.tick, b.tick));
        let basis = Basis::Pair(a.tick, b.tick);
        // Snap check: never blend across a teleport or a jump over the snap distance.
        if b.teleport || a.translation.distance(b.translation) > profile.snap_distance_m {
            return Some(Raw {
                pose: pose_of(a),
                state: ProxyTimelineState::Interpolating,
                bracket,
                t: 0.0,
                basis,
                left: Some(a.tick),
                snap_pair: Some(b.tick),
            });
        }
        // Gap rule: over a long pair, hold the left state and blend over the last interval only.
        // It applies to a left state that is at rest (zero or no velocity): a left state with a
        // velocity is a moving entity whose states were lost, which was shown extrapolating, so
        // the pair blends over its whole span (ADR-1662). A velocity-less left state that moved
        // from the state before it is moving too (spec 120 § 9.7): a budget-deferred mover is
        // not held. The hold and blend of a rest stay on the send interval even when the budget
        // defers the proxy: its next state is known only about one delay before its tick, so a
        // longer blend would hold and then jump.
        let inferred_moving =
            a.velocity.is_none() && spacing.is_some_and(|s| s.velocity.length_squared() > 0.0);
        let span = (b.tick - a.tick) as f64;
        let start = if span > 2.0 * send && moving(a.velocity).is_none() && !inferred_moving {
            b.tick as f64 - send
        } else {
            a.tick as f64
        };
        let blend_ticks = (b.tick as f64 - start).max(1e-9);
        let t = ((interp - start) / blend_ticks).clamp(0.0, 1.0);
        let tf = t as f32;
        let dt_s = (blend_ticks / p.tick_hz.max(1e-6)) as f32;
        let translation = match (profile.interpolation, a.velocity, b.velocity) {
            (InterpolationMode::Hermite, Some(v0), Some(v1)) => {
                let t2 = tf * tf;
                let t3 = t2 * tf;
                let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
                let h10 = t3 - 2.0 * t2 + tf;
                let h01 = -2.0 * t3 + 3.0 * t2;
                let h11 = t3 - t2;
                a.translation * h00 + v0 * (dt_s * h10) + b.translation * h01 + v1 * (dt_s * h11)
            }
            _ => a.translation.lerp(b.translation, tf),
        };
        let velocity = match (a.velocity, b.velocity) {
            (Some(v0), Some(v1)) => Some(v0.lerp(v1, tf)),
            _ => None,
        };
        Some(Raw {
            pose: Pose {
                translation,
                rotation: nlerp(a.rotation, b.rotation, tf),
                scale: a.scale.lerp(b.scale, tf),
                velocity,
            },
            state: ProxyTimelineState::Interpolating,
            bracket,
            t: tf,
            basis,
            left: Some(a.tick),
            snap_pair: None,
        })
    }

    fn basis_pose(
        &self,
        basis: Basis,
        interp: f64,
        profile: &InterpProfile,
        p: &SampleParams,
    ) -> Option<Vec3> {
        match basis {
            Basis::From(s) => {
                let mspt = 1000.0 / p.tick_hz.max(1e-6);
                Some(Self::extrapolated(&s, interp, profile, mspt).0.translation)
            }
            Basis::Inferred(s, cap_ms) => {
                let mspt = 1000.0 / p.tick_hz.max(1e-6);
                Some(Self::extrapolate_by(&s, interp, cap_ms, mspt).0.translation)
            }
            _ => None,
        }
    }

    /// Samples the buffer at `interp_tick` for a frame of `frame_ms` (spec 74 § 5, § 7): finds
    /// the bracketing pair, applies the snap and gap rules, blends, or holds / extrapolates when
    /// starved; a pose change caused by new data after extrapolating becomes a decaying offset.
    /// Drops states older than the left bracket. `None` when nothing is buffered.
    pub fn sample(
        &mut self,
        interp_tick: f64,
        profile: &InterpProfile,
        p: &SampleParams,
        frame_ms: f64,
    ) -> Option<Sample> {
        let raw = self.raw(interp_tick, profile, p)?;
        let half = p.recover_half_life_ms;
        let decay = if half > 0.0 {
            // No libm (spec 71 § 12): f64::powf is not required to return the same bits on
            // glibc, the Windows CRT and bionic, and this factor runs on every client frame.
            crate::scalar::half_decay(frame_ms.max(0.0) / half) as f32
        } else {
            0.0
        };
        self.offset *= decay;
        let mut recovered = false;
        let mut snapped = false;
        if let Some(last) = self.last
            && matches!(
                last.state,
                ProxyTimelineState::Extrapolating | ProxyTimelineState::Starved
            )
            && last.basis != raw.basis
            && let Some(old) = self.basis_pose(last.basis, interp_tick, profile, p)
        {
            // New data after extrapolating: keep the shown pose and let the difference decay.
            self.offset += old - raw.pose.translation;
            recovered = true;
        }
        if let Some(target) = raw.snap_pair {
            self.snap_target = Some(target);
        }
        let left_changed = self.last.and_then(|l| l.left) != raw.left;
        if left_changed && raw.left.is_some() && raw.left == self.snap_target {
            // The proxy reached the far side of a teleport or snap jump: no offset carries over.
            self.snap_target = None;
            self.offset = Vec3::ZERO;
            snapped = true;
        }
        if self.offset.length() > profile.snap_distance_m {
            self.offset = Vec3::ZERO;
            snapped = true;
        }
        if profile.interpolation == InterpolationMode::None {
            self.offset = Vec3::ZERO;
        }
        let mut pose = raw.pose;
        pose.translation += self.offset;
        // States older than the left bracket are no longer needed.
        if let Some(left) = raw.left {
            while self.states.front().is_some_and(|s| s.tick < left) {
                self.dropped = self.states.pop_front();
            }
        }
        self.last = Some(LastFrame {
            state: raw.state,
            basis: raw.basis,
            left: raw.left,
        });
        Some(Sample {
            pose,
            state: raw.state,
            bracket: raw.bracket,
            t: raw.t,
            snapped,
            recovered,
        })
    }
}

// ---- ECS glue -----------------------------------------------------------------------------------

/// Client: the snapshot buffer of a proxy (inserted when its first state arrives under
/// interpolation).
#[derive(Component, Debug, Clone, Default)]
pub struct NetSnapshotBuffer(pub SnapshotBuffer);

/// Client: a proxy past its newest state and its extrapolation limit (removed when data resumes).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetStarved;

/// Client: a proxy whose despawn arrived but whose despawn tick is not shown yet. Gameplay queries
/// on clients should exclude it (`Without<NetPendingDespawn>`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPendingDespawn {
    /// The server tick of the despawn.
    pub tick: NetTick,
}

/// Client: a spawned proxy kept hidden until its spawn tick is shown.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetHiddenUntilShown {
    /// The spawn tick.
    pub tick: NetTick,
    /// The visibility to restore (`None` = the entity had none).
    pub previous: Option<Visibility>,
}

/// One deferred step-component value.
pub type StepApply = Box<dyn FnOnce(&mut World, Entity) + Send + Sync>;

/// Client: received step-component values waiting for the interpolation tick to reach their tick
/// (spec 74 § 3).
#[derive(Component, Default)]
pub struct NetStepQueue {
    items: VecDeque<(u64, StepApply)>,
}

impl std::fmt::Debug for NetStepQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetStepQueue")
            .field("ticks", &self.items.iter().map(|(t, _)| *t).collect::<Vec<_>>())
            .finish()
    }
}

impl NetStepQueue {
    /// The ticks of the queued values, oldest first.
    pub fn ticks(&self) -> Vec<u64> {
        self.items.iter().map(|(t, _)| *t).collect()
    }
}

/// Client: the newest received value of a step or interpolated component on a proxy (deltas decode
/// against it, not against the shown value).
#[derive(Component, Debug, Clone)]
pub struct NetReceived<C: Send + Sync + 'static>(pub C);

/// Client: the received values of an interpolated component, keyed by server tick.
#[derive(Component, Debug, Clone)]
pub struct NetInterpValues<C: Send + Sync + 'static> {
    values: VecDeque<(u64, C)>,
}

impl<C: Send + Sync + 'static> Default for NetInterpValues<C> {
    fn default() -> Self {
        Self {
            values: VecDeque::new(),
        }
    }
}

#[derive(Resource)]
struct InterpLerp<C: Send + Sync + 'static>(fn(&C, &C, f32) -> C);

/// The blend pass of one interpolated component type: `(world, interp_tick)`.
type BlendFn = fn(&mut World, f64);

/// The components registered with [`InterpolateAppExt::interpolate`], and their blend passes.
#[derive(Resource, Default, Clone)]
pub struct InterpolatedComponents {
    list: Vec<(TypeId, &'static str, BlendFn)>,
}

impl std::fmt::Debug for InterpolatedComponents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.list.iter().map(|(_, n, _)| n))
            .finish()
    }
}

impl InterpolatedComponents {
    /// Whether `C` is blended.
    pub fn contains(&self, t: TypeId) -> bool {
        self.list.iter().any(|(id, _, _)| *id == t)
    }

    /// The registered type names.
    pub fn names(&self) -> Vec<&'static str> {
        self.list.iter().map(|(_, n, _)| *n).collect()
    }
}

/// Counters of the interpolation layer (agent `net.interpolation`, tests).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InterpCounters {
    /// Proxy frames shown starved.
    pub frames_starved: u64,
    /// Proxy frames shown extrapolated.
    pub frames_extrapolated: u64,
    /// Recoveries from extrapolation.
    pub recoveries: u64,
    /// Snaps (teleports, jumps over the snap distance, offsets over it).
    pub snaps: u64,
}

/// Client resource: turns proxy interpolation on (opt-in) and holds the clock, the default
/// profile and the counters.
#[derive(Resource, Debug, Clone)]
pub struct NetInterpolation {
    /// The timeline.
    pub clock: InterpClock,
    /// Local time in milliseconds; set by the host before packets are applied (arrival time).
    pub now_ms: f64,
    /// The profile of proxies without [`NetInterpProfile`].
    pub default_profile: InterpProfile,
    /// Counters.
    pub counters: InterpCounters,
    last_frame_ms: Option<f64>,
    snap_times: VecDeque<f64>,
    starved_now: u32,
    extrapolating_now: u32,
}

impl NetInterpolation {
    /// Interpolation with these settings for a server ticking at `tick_hz` that sends state every
    /// `send_interval_ticks` ticks.
    pub fn new(settings: InterpolationSettings, tick_hz: f64, send_interval_ticks: f64) -> Self {
        Self {
            clock: InterpClock::new(settings, tick_hz, send_interval_ticks),
            now_ms: 0.0,
            default_profile: InterpProfile::default(),
            counters: InterpCounters::default(),
            last_frame_ms: None,
            snap_times: VecDeque::new(),
            starved_now: 0,
            extrapolating_now: 0,
        }
    }

    /// Asks for a clock resync on the next frame (call on Android `resumed`, spec 74 § 14).
    pub fn resync(&mut self) {
        self.clock.request_resync();
    }

    /// The sample parameters of this clock.
    pub fn sample_params(&self) -> SampleParams {
        SampleParams {
            tick_hz: self.clock.tick_hz,
            send_interval_ticks: self.clock.send_interval_ticks(),
            recover_half_life_ms: self.clock.settings().recover_half_life_ms,
            newest_tick: self.clock.newest_tick(),
        }
    }
}

/// Installs [`NetInterpolation`] on a client world from its `config/net.ron` settings (the
/// interpolation section), for a server at `tick_hz` sending every `send_interval_ticks` ticks.
pub fn install_interpolation(world: &mut World, tick_hz: f64, send_interval_ticks: f64) {
    let settings = world
        .get_resource::<NetSettings>()
        .map(|s| s.interpolation.clone())
        .unwrap_or_default();
    world.insert_resource(NetInterpolation::new(settings, tick_hz, send_interval_ticks));
}

/// Registers `C` (already replicated, spec 71) as interpolated: proxies blend it with `lerp`
/// every frame instead of stepping it.
pub fn register_interpolated<C: Component + ReplicateCodec + Clone>(
    world: &mut World,
    lerp: fn(&C, &C, f32) -> C,
) {
    let replicated = world
        .get_resource::<ReplicationRegistry>()
        .is_some_and(|r| r.find(std::any::type_name::<C>()).is_some());
    if !replicated {
        tracing::error!(target: "net", component = std::any::type_name::<C>(), "net.interpolate_not_replicated");
    }
    world.insert_resource(InterpLerp::<C>(lerp));
    let mut list = world
        .remove_resource::<InterpolatedComponents>()
        .unwrap_or_default();
    if !list.contains(TypeId::of::<C>()) {
        list.list.push((
            TypeId::of::<C>(),
            std::any::type_name::<C>(),
            blend_component::<C>,
        ));
    }
    world.insert_resource(list);
}

/// `app.interpolate::<C>(lerp)` (spec 74 § 3).
pub trait InterpolateAppExt {
    /// `C` must be replicated (spec 71). `lerp(a, b, t)` with `t` in [0, 1].
    fn interpolate<C: Component + ReplicateCodec + Clone>(&mut self, lerp: fn(&C, &C, f32) -> C)
    -> &mut Self;
}

impl InterpolateAppExt for App {
    fn interpolate<C: Component + ReplicateCodec + Clone>(
        &mut self,
        lerp: fn(&C, &C, f32) -> C,
    ) -> &mut Self {
        register_interpolated::<C>(self.world_mut(), lerp);
        self
    }
}

fn blend_component<C: Component + ReplicateCodec + Clone>(world: &mut World, interp: f64) {
    let Some(lerp) = world.get_resource::<InterpLerp<C>>().map(|l| l.0) else {
        return;
    };
    let mut q = world.query_filtered::<(Entity, &NetInterpValues<C>), (
        Without<NetOwnedLocally>,
        Without<NetPredicted>,
        Without<crate::model::NetInterrupted>,
    )>();
    let mut out: Vec<(Entity, C, Option<u64>)> = Vec::new();
    for (e, vals) in q.iter(world) {
        let ia = vals.values.iter().rposition(|(t, _)| *t as f64 <= interp);
        let value = match ia {
            None => vals.values.front().map(|(_, v)| v.clone()),
            Some(i) => match (vals.values.get(i), vals.values.get(i + 1)) {
                (Some((ta, a)), Some((tb, b))) => {
                    let span = (*tb - *ta).max(1) as f64;
                    let t = ((interp - *ta as f64) / span).clamp(0.0, 1.0) as f32;
                    Some(lerp(a, b, t))
                }
                (Some((_, a)), None) => Some(a.clone()),
                _ => None,
            },
        };
        if let Some(v) = value {
            out.push((e, v, ia.and_then(|i| vals.values.get(i)).map(|(t, _)| *t)));
        }
    }
    for (e, v, left) in out {
        if let Ok(mut ent) = world.get_entity_mut(e) {
            ent.insert(v);
            if let Some(left) = left
                && let Some(mut vals) = ent.get_mut::<NetInterpValues<C>>()
            {
                while vals.values.front().is_some_and(|(t, _)| *t < left) {
                    vals.values.pop_front();
                }
            }
        }
    }
}

/// Whether a client copy is shown through interpolation (a proxy, interpolation installed, its
/// profile not `None`).
pub fn is_interpolated_proxy(world: &World, e: Entity) -> bool {
    let Some(ni) = world.get_resource::<NetInterpolation>() else {
        return false;
    };
    let Ok(ent) = world.get_entity(e) else {
        return false;
    };
    if ent.contains::<NetOwnedLocally>() || ent.contains::<NetPredicted>() {
        return false;
    }
    let profile = ent.get::<NetInterpProfile>().map_or(ni.default_profile, |p| p.0);
    profile.interpolation != InterpolationMode::None
}

fn profile_of(world: &World, e: Entity) -> InterpProfile {
    world
        .get::<NetInterpProfile>(e)
        .map(|p| p.0)
        .or_else(|| {
            world
                .get_resource::<NetInterpolation>()
                .map(|n| n.default_profile)
        })
        .unwrap_or_default()
}

/// Client apply hook, at spawn: a copy that names a loaded `.netprofile.ron` (`NetProfileRef` or
/// `Replicated.profile`, e.g. from its prefab or level data) and has no [`NetInterpProfile`] gets
/// the profile's interpolation fields.
pub(crate) fn adopt_named_profile(world: &mut World, e: Entity) {
    let Ok(ent) = world.get_entity(e) else {
        return;
    };
    if ent.contains::<NetInterpProfile>() {
        return;
    }
    let name = ent
        .get::<crate::profile::NetProfileRef>()
        .map(|r| r.0.clone())
        .or_else(|| {
            ent.get::<crate::replication::Replicated>()
                .and_then(|r| r.profile.clone())
        });
    let Some(name) = name else {
        return;
    };
    let Some(p) = world
        .get_resource::<crate::profile::NetProfiles>()
        .and_then(|ps| ps.get(&name).map(crate::profile::NetProfile::interp_profile))
    else {
        return;
    };
    if let Ok(mut ent) = world.get_entity_mut(e) {
        ent.insert(NetInterpProfile(p));
    }
}

/// Client apply hook: a received transform of an interpolated proxy goes into its buffer.
pub(crate) fn buffer_transform(world: &mut World, e: Entity, state: ProxyState) {
    let cap = world
        .get_resource::<NetInterpolation>()
        .map_or(32, |n| n.clock.settings().buffer_states as usize);
    let Ok(mut ent) = world.get_entity_mut(e) else {
        return;
    };
    match ent.get_mut::<NetSnapshotBuffer>() {
        Some(mut b) => b.0.push(state),
        None => {
            let mut b = SnapshotBuffer::new(cap);
            b.push(state);
            ent.insert(NetSnapshotBuffer(b));
        }
    }
}

/// Client apply hook: a new proxy spawned at `tick` stays hidden until the tick is shown.
pub(crate) fn hide_until_shown(world: &mut World, e: Entity, tick: NetTick) {
    if profile_of(world, e).appear_immediately {
        return;
    }
    let shown = world
        .get_resource::<NetInterpolation>()
        .and_then(|n| n.clock.interp_tick())
        .is_some_and(|it| it >= tick.0 as f64);
    if shown {
        return;
    }
    let Ok(mut ent) = world.get_entity_mut(e) else {
        return;
    };
    let previous = ent.get::<Visibility>().copied();
    ent.insert((Visibility::Hidden, NetHiddenUntilShown { tick, previous }));
}

/// Client apply hook: queues a step value (`apply` runs when the interpolation tick reaches
/// `tick`).
pub(crate) fn queue_step(world: &mut World, e: Entity, tick: u64, apply: StepApply) {
    let Ok(mut ent) = world.get_entity_mut(e) else {
        return;
    };
    match ent.get_mut::<NetStepQueue>() {
        Some(mut q) => q.items.push_back((tick, apply)),
        None => {
            let mut q = NetStepQueue::default();
            q.items.push_back((tick, apply));
            ent.insert(q);
        }
    }
}

/// Client apply hook: a copy that is no longer an interpolated proxy (it became owned or
/// predicted) applies its queued step values at once, in order, before newer state lands on it,
/// so its components hold the newest received values again.
pub(crate) fn flush_steps(world: &mut World, e: Entity) {
    let Some(queue) = world
        .get_mut::<NetStepQueue>(e)
        .map(|mut q| std::mem::take(&mut q.items))
    else {
        return;
    };
    for (_, f) in queue {
        f(world, e);
    }
    if let Ok(mut ent) = world.get_entity_mut(e) {
        ent.remove::<NetStepQueue>();
    }
}

/// Client apply hook: a spawn record for a copy that exists replaces its state, so values queued
/// for earlier ticks are dropped unapplied.
pub(crate) fn drop_steps(world: &mut World, e: Entity) {
    if let Ok(mut ent) = world.get_entity_mut(e) {
        ent.remove::<NetStepQueue>();
    }
}

/// Client apply hook: a received value of an interpolated component.
pub(crate) fn push_interp_value<C: Component + Clone>(world: &mut World, e: Entity, tick: u64, value: C) {
    let cap = world
        .get_resource::<NetInterpolation>()
        .map_or(32, |n| n.clock.settings().buffer_states as usize);
    let Ok(mut ent) = world.get_entity_mut(e) else {
        return;
    };
    if ent.get::<NetInterpValues<C>>().is_none() {
        ent.insert(NetInterpValues::<C>::default());
    }
    if let Some(mut vals) = ent.get_mut::<NetInterpValues<C>>() {
        let pos = vals.values.iter().position(|(t, _)| *t >= tick);
        match pos {
            Some(i) if vals.values.get(i).is_some_and(|(t, _)| *t == tick) => {
                if let Some(slot) = vals.values.get_mut(i) {
                    slot.1 = value;
                }
            }
            Some(i) => vals.values.insert(i, (tick, value)),
            None => vals.values.push_back((tick, value)),
        }
        while vals.values.len() > cap.max(2) {
            vals.values.pop_front();
        }
    }
}

/// Whether `C` is registered as interpolated on this world.
pub(crate) fn is_interpolated_type<C: 'static>(world: &World) -> bool {
    world
        .get_resource::<InterpolatedComponents>()
        .is_some_and(|l| l.contains(TypeId::of::<C>()))
}

/// Client apply hook: whether a despawn at `tick` waits for the interpolation tick (and marks
/// the entity). `false` = despawn now.
pub(crate) fn defer_despawn(world: &mut World, e: Entity, tick: NetTick) -> bool {
    if !would_defer_despawn(world, e, tick) {
        return false;
    }
    if let Ok(mut ent) = world.get_entity_mut(e) {
        ent.insert(NetPendingDespawn { tick });
        return true;
    }
    false
}

/// Whether a despawn with this tick waits for the interpolation tick (an interpolated proxy whose
/// despawn tick is not shown yet).
pub(crate) fn would_defer_despawn(world: &World, e: Entity, tick: NetTick) -> bool {
    is_interpolated_proxy(world, e)
        && !world
            .get_resource::<NetInterpolation>()
            .and_then(|n| n.clock.interp_tick())
            .is_some_and(|it| it >= tick.0 as f64)
}

/// Client apply hook: a state frame for `tick` arrived (now, at [`NetInterpolation::now_ms`]).
pub(crate) fn on_state_frame(world: &mut World, tick: NetTick) {
    if let Some(mut ni) = world.get_resource_mut::<NetInterpolation>() {
        let now = ni.now_ms;
        ni.clock.on_state(u64::from(tick.0), now);
    }
}

/// Client apply hook: a state frame older than the newest arrived (reordered on the state
/// channel). It fills a loss gap of the clock (spec 120 § 9.7) without an arrival sample.
pub(crate) fn on_late_state_frame(world: &mut World, tick: NetTick) {
    if let Some(mut ni) = world.get_resource_mut::<NetInterpolation>() {
        ni.clock.on_late_state(u64::from(tick.0));
    }
}

/// The per-frame interpolation pass on a client (spec 74 § 5-7, span `net_interp`): advances the
/// clock to `now_ms`, shows every proxy at the interpolation tick, applies step components whose
/// tick is reached, blends interpolated components, reveals proxies whose spawn tick is shown and
/// despawns those whose despawn tick is. Does nothing without [`NetInterpolation`] or on a server.
pub fn net_interpolate(world: &mut World, now_ms: f64) {
    let _span = tracing::trace_span!("net_interp").entered();
    if world.get_resource::<NetMode>().is_some_and(|m| m.is_server()) {
        return;
    }
    let Some(mut ni) = world.remove_resource::<NetInterpolation>() else {
        return;
    };
    ni.now_ms = now_ms;
    let frame_ms = ni.last_frame_ms.map_or(0.0, |l| (now_ms - l).max(0.0));
    ni.last_frame_ms = Some(now_ms);
    let Some(interp) = ni.clock.advance(now_ms) else {
        world.insert_resource(ni);
        return;
    };
    world.insert_resource(NetInterpTick(NetTick(
        interp.clamp(0.0, f64::from(u32::MAX)).floor() as u32,
    )));
    let params = ni.sample_params();

    // Transforms.
    let mut q = world.query_filtered::<(
        Entity,
        &mut NetSnapshotBuffer,
        Option<&NetInterpProfile>,
        Option<&NetTransformMode>,
        Has<NetStarved>,
    ), (
        Without<NetOwnedLocally>,
        Without<NetPredicted>,
        Without<NetOutOfScope>,
        Without<crate::model::NetInterrupted>,
    )>();
    let mut writes: Vec<(Entity, Pose, TransformReplication, bool, bool)> = Vec::new();
    ni.starved_now = 0;
    ni.extrapolating_now = 0;
    for (e, mut buf, profile, mode, was_starved) in q.iter_mut(world) {
        let profile = profile.map_or(ni.default_profile, |p| p.0);
        let Some(s) = buf.0.sample(interp, &profile, &params, frame_ms) else {
            continue;
        };
        let starved = s.state == ProxyTimelineState::Starved;
        match s.state {
            ProxyTimelineState::Starved => {
                ni.counters.frames_starved += 1;
                ni.starved_now += 1;
            }
            ProxyTimelineState::Extrapolating => {
                ni.counters.frames_extrapolated += 1;
                ni.extrapolating_now += 1;
            }
            _ => {}
        }
        if s.recovered {
            ni.counters.recoveries += 1;
        }
        if s.snapped {
            ni.counters.snaps += 1;
            ni.snap_times.push_back(now_ms);
        }
        let mode = mode.map_or(TransformReplication::Movement, |m| m.0);
        writes.push((e, s.pose, mode, starved, starved != was_starved));
    }
    while ni.snap_times.front().is_some_and(|t| now_ms - *t > 1000.0) {
        ni.snap_times.pop_front();
    }
    for (e, pose, mode, starved, flip) in writes {
        let Ok(mut ent) = world.get_entity_mut(e) else {
            continue;
        };
        let mut tr = ent.get::<Transform>().copied().unwrap_or_default();
        tr.translation = pose.translation;
        if matches!(mode, TransformReplication::Movement | TransformReplication::Full) {
            tr.rotation = pose.rotation;
        }
        if mode == TransformReplication::Full {
            tr.scale = pose.scale;
        }
        ent.insert(tr);
        if let Some(v) = pose.velocity {
            ent.insert(NetVelocity(v));
        } else if crate::replication::is_replication_owned_velocity(&ent) {
            // The shown pose has no replicated velocity (a far snapshot, a pair with one, a
            // velocity-less proxy's guess): the old one no longer holds (ADR-10215). The query
            // leaves out owned and predicted copies; the check keeps the rule in one place.
            ent.remove::<NetVelocity>();
        }
        if flip {
            if starved {
                ent.insert(NetStarved);
            } else {
                ent.remove::<NetStarved>();
            }
        }
    }

    // Step components whose tick is shown.
    let mut q = world.query::<(Entity, &mut NetStepQueue)>();
    let mut due: Vec<(Entity, StepApply)> = Vec::new();
    for (e, mut queue) in q.iter_mut(world) {
        while queue.items.front().is_some_and(|(t, _)| *t as f64 <= interp) {
            if let Some((_, f)) = queue.items.pop_front() {
                due.push((e, f));
            }
        }
    }
    for (e, f) in due {
        if world.get_entity(e).is_ok() {
            f(world, e);
        }
    }

    // Interpolated components.
    let blends = world
        .get_resource::<InterpolatedComponents>()
        .map(|l| l.list.iter().map(|(_, _, f)| *f).collect::<Vec<_>>())
        .unwrap_or_default();
    for f in blends {
        f(world, interp);
    }

    // Spawn reveal and delayed despawn.
    let mut q = world.query::<(Entity, &NetHiddenUntilShown)>();
    let reveal: Vec<(Entity, Option<Visibility>)> = q
        .iter(world)
        .filter(|(_, h)| h.tick.0 as f64 <= interp)
        .map(|(e, h)| (e, h.previous))
        .collect();
    for (e, previous) in reveal {
        if let Ok(mut ent) = world.get_entity_mut(e) {
            ent.remove::<NetHiddenUntilShown>();
            match previous {
                Some(v) => {
                    ent.insert(v);
                }
                None => {
                    ent.insert(Visibility::Inherited);
                }
            }
        }
    }
    let mut q = world.query::<(Entity, &NetPendingDespawn)>();
    let gone: Vec<Entity> = q
        .iter(world)
        .filter(|(_, p)| p.tick.0 as f64 <= interp)
        .map(|(e, _)| e)
        .collect();
    for e in gone {
        // Replicated children have their own records: they outlive the parent's copy and wait
        // for it to come back (as an immediate despawn does).
        let parent = world.get::<NetId>(e).copied();
        let kids: Vec<Entity> = world
            .get::<Children>(e)
            .map(|c| {
                c.iter()
                    .filter(|k| {
                        world.get::<NetId>(*k).is_some() && world.get::<NetPendingDespawn>(*k).is_none()
                    })
                    .collect()
            })
            .unwrap_or_default();
        for k in kids {
            if let Ok(mut ent) = world.get_entity_mut(k) {
                ent.remove::<ChildOf>();
            }
            if let (Some(p), Some(mut client)) = (
                parent,
                world.get_resource_mut::<crate::replication::ClientReplication>(),
            ) {
                client.push_pending_parent(k, p);
            }
        }
        let _ = world.despawn(e);
    }
    world.insert_resource(ni);
}

// ---- view tick (spec 74 § 8) --------------------------------------------------------------------

/// Sub-tick steps of the reported view tick.
pub const VIEW_TICK_SCALE: f64 = 256.0;

/// The view tick an input command carries: the client's `interp_tick` when the command's tick
/// was sampled, in 1/256 tick, plus the delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewTickWire {
    /// `interp_tick × 256`, rounded.
    pub tick_256: u64,
    /// The interpolation delay, in milliseconds (saturating).
    pub delay_ms: u16,
}

impl ViewTickWire {
    /// Quantizes a fractional interpolation tick and a delay.
    pub fn new(interp_tick: f64, delay_ms: f64) -> Self {
        Self {
            tick_256: view_tick_to_wire(interp_tick),
            delay_ms: delay_ms.clamp(0.0, f64::from(u16::MAX)).round() as u16,
        }
    }

    /// The fractional tick.
    pub fn tick(&self) -> f64 {
        view_tick_from_wire(self.tick_256)
    }

    /// Writes it (varint + 16 bits).
    pub fn encode(&self, w: &mut BitWriter) {
        w.write_varint(self.tick_256);
        w.write_bits(u64::from(self.delay_ms), 16);
    }

    /// Reads it.
    pub fn decode(r: &mut BitReader) -> Result<Self, DecodeError> {
        let tick_256 = r.read_varint()?;
        let delay_ms = r.read_bits(16)? as u16;
        Ok(Self { tick_256, delay_ms })
    }
}

/// `interp_tick` in 1/256 tick (negative ticks clamp to 0).
pub fn view_tick_to_wire(interp_tick: f64) -> u64 {
    if interp_tick.is_finite() {
        (interp_tick.max(0.0) * VIEW_TICK_SCALE).round() as u64
    } else {
        0
    }
}

/// Back from 1/256 tick.
pub fn view_tick_from_wire(wire: u64) -> f64 {
    wire as f64 / VIEW_TICK_SCALE
}

/// Server: the view tick of a player (on its `Player` entity), from the input command consumed
/// for the current tick. Spec 75 rewinds hit checks to it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct NetViewTick {
    /// The fractional server tick the player saw.
    pub tick: f64,
    /// The player's interpolation delay, in milliseconds.
    pub delay_ms: f64,
}

/// Server: called when a player's command for the current tick is consumed. `None` (a repeated
/// command for missing input) keeps the previous value.
///
/// On a server the carried tick is checked against the player's measured rewind window first
/// (spec 120 § 9.6, `lagcomp::window`): a tick older than the window, newer than the present, or
/// running backward past the allowance is clamped, counted, and the clamped tick is kept.
pub fn set_view_tick(world: &mut World, player: Entity, wire: Option<ViewTickWire>) {
    let Some(w) = wire else {
        return;
    };
    if world.get_entity(player).is_err() {
        return;
    }
    let tick = crate::lagcomp::window::check_view_tick(world, player, w.tick(), f64::from(w.delay_ms));
    if let Ok(mut ent) = world.get_entity_mut(player) {
        ent.insert(NetViewTick {
            tick,
            delay_ms: f64::from(w.delay_ms),
        });
    }
}

// ---- agent data ---------------------------------------------------------------------------------

/// Why an interpolation query failed (agent error codes, `book/agents/net-interpolation.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpQueryError {
    /// The process is a server or offline: `net_not_client`.
    NotClient,
    /// The entity is not an interpolated proxy: `entity_not_proxy`.
    NotProxy,
    /// No interpolation is running on this client (not installed, or no state yet):
    /// `net_not_running`.
    NotRunning,
    /// A fixed delay outside 0..=1,000 ms: `delay_out_of_range`.
    DelayOutOfRange,
}

impl InterpQueryError {
    /// The protocol error code.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotClient => "net_not_client",
            Self::NotProxy => "entity_not_proxy",
            Self::NotRunning => "net_not_running",
            Self::DelayOutOfRange => "delay_out_of_range",
        }
    }
}

/// `net.interpolation`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct InterpolationReport {
    /// The fractional interpolation tick.
    pub interp_tick: f64,
    /// The current delay, in milliseconds.
    pub delay_ms: f64,
    /// The delay the clock steers toward, in milliseconds.
    pub target_delay_ms: f64,
    /// Arrival jitter, in milliseconds.
    pub jitter_ms: f64,
    /// The loss term of the target delay (the measured burst-loss gap, spec 120 § 9.7), in
    /// milliseconds.
    pub loss_gap_ms: f64,
    /// Clock speed (1.0 = real time).
    pub clock_rate: f64,
    /// Proxies starved on the last frame.
    pub starved: u32,
    /// Proxies extrapolating on the last frame.
    pub extrapolating: u32,
    /// Snaps over the last second.
    pub snaps_per_s: u32,
    /// Clock resyncs so far.
    pub resyncs: u64,
}

/// `net.proxy_timeline`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProxyTimeline {
    /// Buffered state ticks, oldest first.
    pub buffered: Vec<u64>,
    /// The bracketing pair at the current interpolation tick.
    pub bracket: Option<(u64, u64)>,
    /// Blend parameter within the bracket.
    pub t: f32,
    /// `interpolating`, `extrapolating`, `starved` or `holding`.
    pub state: &'static str,
    /// Ticks between the proxy's two newest states (its own update interval; larger than the
    /// send interval when the server's budget defers it, spec 120 § 9.7), when known.
    pub update_interval_ticks: Option<u64>,
}

fn running(world: &World) -> Result<&NetInterpolation, InterpQueryError> {
    if world.get_resource::<NetMode>().is_none_or(|m| !m.is_client()) {
        return Err(InterpQueryError::NotClient);
    }
    world
        .get_resource::<NetInterpolation>()
        .ok_or(InterpQueryError::NotRunning)
}

/// Data of `net.interpolation` (client).
pub fn interpolation_report(world: &World) -> Result<InterpolationReport, InterpQueryError> {
    let ni = running(world)?;
    let interp_tick = ni.clock.interp_tick().ok_or(InterpQueryError::NotRunning)?;
    Ok(InterpolationReport {
        interp_tick,
        delay_ms: ni.clock.delay_ms(ni.now_ms).unwrap_or(0.0),
        target_delay_ms: ni.clock.target_delay_ms(),
        jitter_ms: ni.clock.jitter_ms(),
        loss_gap_ms: ni.clock.loss_gap_ms(),
        clock_rate: ni.clock.clock_rate(),
        starved: ni.starved_now,
        extrapolating: ni.extrapolating_now,
        snaps_per_s: ni.snap_times.len() as u32,
        resyncs: ni.clock.resyncs(),
    })
}

/// Data of `net.proxy_timeline` (client).
pub fn proxy_timeline(world: &World, entity: Entity) -> Result<ProxyTimeline, InterpQueryError> {
    let ni = running(world)?;
    let Some(buf) = world.get::<NetSnapshotBuffer>(entity) else {
        return Err(InterpQueryError::NotProxy);
    };
    if !is_interpolated_proxy(world, entity) {
        return Err(InterpQueryError::NotProxy);
    }
    let interp = ni.clock.interp_tick().ok_or(InterpQueryError::NotRunning)?;
    let profile = profile_of(world, entity);
    let raw = buf.0.raw(interp, &profile, &ni.sample_params());
    Ok(ProxyTimeline {
        buffered: buf.0.ticks(),
        bracket: raw.as_ref().and_then(|r| r.bracket),
        t: raw.as_ref().map_or(0.0, |r| r.t),
        state: buf.0.last_state().as_str(),
        update_interval_ticks: buf.0.update_interval_ticks(),
    })
}

/// `net.set_interp_delay` (client, dev builds): a fixed delay in milliseconds, or `None` to
/// return to the adaptive delay. Returns the delay the clock now steers toward.
pub fn set_interp_delay(world: &mut World, ms: Option<f64>) -> Result<f64, InterpQueryError> {
    running(world)?;
    if let Some(ms) = ms
        && !(0.0..=1000.0).contains(&ms)
    {
        return Err(InterpQueryError::DelayOutOfRange);
    }
    let mut ni = world
        .get_resource_mut::<NetInterpolation>()
        .ok_or(InterpQueryError::NotRunning)?;
    ni.clock.set_delay_override(ms);
    Ok(ni.clock.target_delay_ms())
}

// ---- plugin -------------------------------------------------------------------------------------

/// Turns proxy interpolation on for a client app: inserts [`NetInterpolation`] (from
/// `config/net.ron`'s interpolation section), stamps arrival times from `RealTime` before packets
/// are received, and runs [`net_interpolate`] in `NetSet::Interpolate` (`PostUpdate`). Add it
/// after `NetPlugin`.
#[derive(Debug, Clone, Copy)]
pub struct NetInterpolationPlugin {
    /// Server ticks per second.
    pub tick_hz: f64,
    /// Server ticks between state packets.
    pub send_interval_ticks: f64,
}

impl Default for NetInterpolationPlugin {
    fn default() -> Self {
        Self {
            tick_hz: 60.0,
            send_interval_ticks: 1.0,
        }
    }
}

fn real_now_ms(world: &World) -> f64 {
    world
        .get_resource::<RealTime>()
        .map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0)
}

fn stamp_arrival_system(world: &mut World) {
    // A replay passes playback time itself (spec 103 § 8).
    if world.contains_resource::<crate::recorder::NetReplayClock>() {
        return;
    }
    let now = real_now_ms(world);
    if let Some(mut ni) = world.get_resource_mut::<NetInterpolation>() {
        ni.now_ms = now;
    }
}

fn interpolate_system(world: &mut World) {
    if world.contains_resource::<crate::recorder::NetReplayClock>() {
        return;
    }
    let now = real_now_ms(world);
    net_interpolate(world, now);
}

impl Plugin for NetInterpolationPlugin {
    fn build(&self, app: &mut App) {
        install_interpolation(app.world_mut(), self.tick_hz, self.send_interval_ticks);
        app.add_systems(PreUpdate, stamp_arrival_system.before(NetSet::Receive));
        app.add_systems(PostUpdate, interpolate_system.in_set(NetSet::Interpolate));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(tick: u64, x: f32, v: Option<f32>) -> ProxyState {
        ProxyState {
            tick,
            translation: Vec3::new(x, 0.0, 0.0),
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            velocity: v.map(|v| Vec3::new(v, 0.0, 0.0)),
            teleport: false,
            steady: false,
        }
    }

    /// Spec 78 § 12.2, ADR-2022: after a local rebase the buffer samples the same world motion,
    /// moved by the shift, with no jump.
    #[test]
    fn a_rebase_moves_the_buffered_states_with_the_origin() {
        let mut b = SnapshotBuffer::new(32);
        for t in [10u64, 12, 14, 16] {
            b.push(st(t, 4000.0 + t as f32, None));
        }
        let p = SampleParams::default();
        let before = b
            .sample(13.0, &InterpProfile::default(), &p, 16.0)
            .expect("sample")
            .pose
            .translation;
        b.shift_origin(Vec3::new(-4096.0, 0.0, 0.0));
        let after = b
            .sample(13.5, &InterpProfile::default(), &p, 16.0)
            .expect("sample")
            .pose
            .translation;
        assert!((before.x - 4013.0).abs() < 1e-3, "{before}");
        assert!((after.x - (4013.5 - 4096.0)).abs() < 1e-3, "{after}");
        assert!(b.states().all(|s| s.translation.x < 0.0));
        // Bad input moves nothing.
        b.shift_origin(Vec3::new(f32::NAN, 0.0, 0.0));
        assert!(b.states().all(|s| s.translation.x.is_finite()));
    }

    /// ADR-8522: a steady state (the server keeps the copy within its extrapolation gate) is
    /// extrapolated past `max_extrapolate_ms` without going `Starved`, also with
    /// `max_extrapolate_ms: 0`; a state that is not steady still starves at the limit. With
    /// `interpolation: None` a steady state moves along its velocity once the clock passes it.
    #[test]
    fn a_steady_state_extrapolates_past_the_limit() {
        let p = SampleParams::default();
        let profile = InterpProfile::default();
        // 100 ms limit; 60 ticks (1 s) past the newest state.
        let mut starving = SnapshotBuffer::new(8);
        starving.push(st(10, 0.0, Some(2.0)));
        let s = starving.sample(70.0, &profile, &p, 16.0).expect("sample");
        assert_eq!(s.state, ProxyTimelineState::Starved);
        assert!(
            (s.pose.translation.x - 0.2).abs() < 1e-4,
            "{}",
            s.pose.translation
        );
        for max in [100.0f32, 0.0] {
            let profile = InterpProfile {
                max_extrapolate_ms: max,
                ..InterpProfile::default()
            };
            let mut steady = SnapshotBuffer::new(8);
            steady.push(ProxyState {
                steady: true,
                ..st(10, 0.0, Some(2.0))
            });
            let s = steady.sample(70.0, &profile, &p, 16.0).expect("sample");
            assert_eq!(s.state, ProxyTimelineState::Extrapolating, "max {max}");
            assert!(
                (s.pose.translation.x - 2.0).abs() < 1e-4,
                "{}",
                s.pose.translation
            );
        }
        // A steady state at rest holds.
        let mut rest = SnapshotBuffer::new(8);
        rest.push(ProxyState {
            steady: true,
            ..st(10, 1.0, Some(0.0))
        });
        let s = rest.sample(70.0, &profile, &p, 16.0).expect("sample");
        assert_eq!(s.state, ProxyTimelineState::Holding);
        // `interpolation: None`: the newest steady state moves; one that is not steady holds.
        let none = InterpProfile {
            interpolation: InterpolationMode::None,
            ..InterpProfile::default()
        };
        let mut b = SnapshotBuffer::new(8);
        b.push(ProxyState {
            steady: true,
            ..st(10, 0.0, Some(3.0))
        });
        let s = b.sample(40.0, &none, &p, 16.0).expect("sample");
        assert_eq!(s.state, ProxyTimelineState::Extrapolating);
        assert!(
            (s.pose.translation.x - 1.5).abs() < 1e-4,
            "{}",
            s.pose.translation
        );
        let s = b.sample(5.0, &none, &p, 16.0).expect("sample");
        assert_eq!(s.state, ProxyTimelineState::Holding, "before its tick it holds");
        let mut b = SnapshotBuffer::new(8);
        b.push(st(10, 0.0, Some(3.0)));
        let s = b.sample(40.0, &none, &p, 16.0).expect("sample");
        assert_eq!(s.state, ProxyTimelineState::Holding);
        assert_eq!(s.pose.translation.x, 0.0);
    }

    /// The Welcome rate replaces the clock's: arrivals of a 128 Hz server no longer drift the
    /// offset estimate by 8.9 ms per tick, so no resync fires.
    #[test]
    fn welcome_rate_keeps_the_arrival_estimate_steady() {
        let mut c = InterpClock::new(InterpolationSettings::default(), 60.0, 1.0);
        c.set_server_rate(128.0, 2.0);
        assert!((c.ms_per_tick() - 1000.0 / 128.0).abs() < 1e-9);
        assert!((c.send_interval_ticks() - 2.0).abs() < 1e-9);
        for k in 0..400u64 {
            let tick = k * 2;
            c.on_state(tick, 5000.0 + tick as f64 * 1000.0 / 128.0);
        }
        assert_eq!(c.resyncs, 0);
        assert!(c.jitter_ms < 1e-6, "{}", c.jitter_ms);
        c.set_server_rate(f64::NAN, 1.0);
        assert!(
            (c.ms_per_tick() - 1000.0 / 128.0).abs() < 1e-9,
            "a bad rate keeps the old one"
        );
    }

    #[test]
    fn bracketing_blends_and_drops_old_states() {
        let mut b = SnapshotBuffer::new(32);
        for t in [10u64, 12, 14, 16] {
            b.push(st(t, t as f32, None));
        }
        let p = SampleParams::default();
        let s = b
            .sample(13.0, &InterpProfile::default(), &p, 16.0)
            .expect("sample");
        assert_eq!(s.bracket, Some((12, 14)));
        assert!((s.pose.translation.x - 13.0).abs() < 1e-5);
        assert_eq!(s.state, ProxyTimelineState::Interpolating);
        assert_eq!(
            b.ticks(),
            vec![12, 14, 16],
            "states before the left bracket are dropped"
        );
    }

    #[test]
    fn reordered_and_duplicate_states_keep_tick_order() {
        let mut b = SnapshotBuffer::new(4);
        for t in [14u64, 10, 12, 12, 16, 18] {
            b.push(st(t, t as f32, None));
        }
        assert_eq!(
            b.ticks(),
            vec![12, 14, 16, 18],
            "ring keeps the newest 4 in order"
        );
    }

    #[test]
    fn rotation_takes_the_shorter_arc() {
        let a = Quat::from_rotation_y(0.1);
        let b = -Quat::from_rotation_y(0.3);
        let q = nlerp(a, b, 0.5);
        assert!(q.angle_between(Quat::from_rotation_y(0.2)) < 1e-3);
    }

    #[test]
    fn view_tick_round_trip_within_a_256th() {
        for i in 0..10_000u64 {
            let t = i as f64 * 0.731 + 0.0013;
            let w = ViewTickWire::new(t, 66.7);
            let mut bw = BitWriter::new();
            w.encode(&mut bw);
            let bytes = bw.finish();
            let back = ViewTickWire::decode(&mut BitReader::new(&bytes)).expect("decode");
            assert_eq!(back, w);
            assert!((back.tick() - t).abs() <= 0.5 / VIEW_TICK_SCALE + 1e-12);
            assert_eq!(back.delay_ms, 67);
        }
        assert_eq!(view_tick_to_wire(-3.0), 0);
        assert_eq!(view_tick_to_wire(f64::NAN), 0);
    }

    /// A seeded uniform in `[0, 1)` (xorshift64*), so the spike tests need no RNG crate.
    fn uniform(state: &mut u64) -> f64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Feeds two clocks the same arrivals (30 Hz state at 60 Hz ticks, 40 ms path delay, 0-20 ms
    /// uniform jitter) except arrival 200: the spiked clock gets it `spike_ms` late, the control
    /// never gets it (the best a clock can do with a delayed packet is what it does with a lost
    /// one). Counts the arrivals after which the estimate or the target delay of the two differ
    /// by more than 0.5 ms.
    fn arrivals_moved_by_spike(spike_ms: f64) -> usize {
        let mut control = InterpClock::new(InterpolationSettings::default(), 60.0, 2.0);
        let mut spiked = control.clone();
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut moved = 0;
        for i in 0..400usize {
            let tick = 2 * i as u64;
            let at = tick as f64 * 1000.0 / 60.0 + 40.0 + 20.0 * uniform(&mut rng);
            if i == 200 {
                spiked.on_state(tick, at + spike_ms);
            } else {
                control.on_state(tick, at);
                spiked.on_state(tick, at);
            }
            let now = at + 1.0;
            let est = |c: &InterpClock| c.server_tick_estimate(now).unwrap_or(0.0) * c.ms_per_tick();
            if (est(&control) - est(&spiked)).abs() > 0.5
                || (control.target_delay_ms() - spiked.target_delay_ms()).abs() > 0.5
            {
                moved += 1;
            }
        }
        moved
    }

    /// Spec 120 § 9.1 / N207 bar: one state packet delayed 300 ms moves the interpolation
    /// estimate and delay for at most 8 arrivals against a clock that never got that packet (the
    /// 64-arrival mean moved them for 64).
    #[test]
    fn one_300_ms_spike_moves_the_delay_for_at_most_8_samples() {
        let moved = arrivals_moved_by_spike(300.0);
        assert!(
            moved <= 8,
            "one 300 ms spike moved the delay for {moved} arrivals"
        );
    }

    /// Spec 120 § 9.1 "0 backward steps on every path", the interpolation clock: bad-wifi-like
    /// arrivals (0-60 ms jitter, 5 % loss) through an rtt-step (+105 ms at 20 s, back at 40 s),
    /// the estimate read every 16 ms frame never goes backward.
    #[test]
    fn the_interp_estimate_never_steps_backward_through_jitter_and_steps() {
        let mut c = InterpClock::new(InterpolationSettings::default(), 60.0, 2.0);
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut arrivals = Vec::new();
        for i in 0..1_800u64 {
            let sent = 2.0 * i as f64 * 1000.0 / 60.0;
            let step = if (20_000.0..40_000.0).contains(&sent) {
                105.0
            } else {
                0.0
            };
            if uniform(&mut rng) < 0.05 {
                continue;
            }
            arrivals.push((sent + 20.0 + step + 60.0 * uniform(&mut rng), 2 * i));
        }
        arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut next = 0;
        let mut last = f64::MIN;
        let mut backward = 0;
        let mut frame = 0.0;
        while frame < 60_000.0 {
            while next < arrivals.len() && arrivals[next].0 <= frame {
                c.on_state(arrivals[next].1, arrivals[next].0);
                next += 1;
            }
            if let Some(est) = c.server_tick_estimate(frame) {
                if est < last {
                    backward += 1;
                }
                last = est;
            }
            frame += 16.0;
        }
        assert_eq!(backward, 0, "the interpolation estimate stepped backward");
    }

    /// A lasting rise in path delay is followed as the window fills (its first late arrival is
    /// held until the second confirms it) and never faster than the slew, so the estimate never
    /// steps back; a fall is followed at once.
    #[test]
    fn the_offset_follows_a_delay_step_without_stepping_back() {
        let mut c = InterpClock::new(InterpolationSettings::default(), 60.0, 2.0);
        let mspt = c.ms_per_tick();
        let mut i = 0u64;
        let mut feed = |c: &mut InterpClock, delay: f64, n: u64| {
            let mut est_prev = f64::MIN;
            for _ in 0..n {
                let at = 2.0 * i as f64 * mspt + delay;
                c.on_state(2 * i, at);
                let est = c.server_tick_estimate(at).expect("running");
                assert!(est >= est_prev, "estimate stepped back at arrival {i}");
                est_prev = est;
                i += 1;
            }
        };
        feed(&mut c, 20.0, 100);
        assert!((c.offset_ms - 20.0).abs() < 1e-9);
        feed(&mut c, 125.0, 1);
        assert!(
            (c.offset_ms - 20.0).abs() < 1e-9,
            "a lone late arrival is held: {}",
            c.offset_ms
        );
        feed(&mut c, 125.0, 1);
        assert!(c.offset_ms > 20.0, "the second confirms it: {}", c.offset_ms);
        feed(&mut c, 125.0, 80);
        assert!(
            (c.offset_ms - 125.0).abs() < 1e-9,
            "followed the rise: {}",
            c.offset_ms
        );
        feed(&mut c, 20.0, 1);
        assert!(c.offset_ms < 125.0, "followed the fall at once: {}", c.offset_ms);
    }

    #[test]
    fn clock_starts_at_the_target_delay_and_is_monotonic() {
        let mut c = InterpClock::new(InterpolationSettings::default(), 60.0, 2.0);
        assert_eq!(c.advance(0.0), None);
        c.on_state(100, 1000.0);
        let first = c.advance(1000.0).expect("running");
        let target = c.target_delay_ms();
        assert!(
            (target - 66.666).abs() < 0.1,
            "2 send intervals at 30 Hz: {target}"
        );
        assert!((c.delay_ms(1000.0).expect("delay") - target).abs() < 1e-6);
        let mut prev = first;
        for i in 1..600 {
            let now = 1000.0 + i as f64 * 16.666;
            if i % 2 == 0 {
                c.on_state(100 + i as u64, now);
            }
            let it = c.advance(now).expect("running");
            assert!(it >= prev);
            prev = it;
        }
    }

    #[test]
    fn errors_have_protocol_codes() {
        let codes = [
            InterpQueryError::NotClient,
            InterpQueryError::NotProxy,
            InterpQueryError::NotRunning,
            InterpQueryError::DelayOutOfRange,
        ]
        .map(|e| e.code());
        assert_eq!(
            codes,
            [
                "net_not_client",
                "entity_not_proxy",
                "net_not_running",
                "delay_out_of_range"
            ]
        );
        for c in codes {
            assert!(c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }
}

#[cfg(test)]
mod det_tests {
    //! Cross-platform blend determinism (spec 71 § 12): the rotation written to a proxy's
    //! `Transform` must be bit-identical on x86_64 and aarch64. `nlerp`'s normalization and the
    //! recovery-offset decay must not go through glam's SIMD dot4 (SSE2 pairs `(x + z) + (y + w)`,
    //! NEON reduces pairwise) or `libm`'s `powf` (not identical across glibc, the Windows CRT and
    //! bionic). `ADVERSARIAL` is a quaternion whose sum of squares rounds to three different bit
    //! patterns under the three horizontal-sum orderings (found by search over random bit
    //! patterns).
    use super::*;
    use crate::scalar::half_decay;

    /// Four components whose sum of squares rounds to three different bit patterns under the
    /// three horizontal-sum orderings (SSE2 `(x + z) + (y + w)`, NEON pairwise `(x + y) + (z + w)`,
    /// scalar left-to-right), so reverting the normalization to glam's SIMD normalize fails on
    /// x86_64 *and* aarch64.
    const ADVERSARIAL: [f32; 4] = [0.396_278_26, 0.367_807_63, 0.041_129_23, 0.353_028_54];

    /// The independent reference: one left-to-right sum of squares, one `sqrt`, one division per
    /// component (the order `crate::scalar::normalize4` uses, written again so the test pins it).
    fn lr_normalize(q: [f32; 4]) -> [f32; 4] {
        let len2 = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
        let len = len2.sqrt();
        [q[0] / len, q[1] / len, q[2] / len, q[3] / len]
    }

    fn bits(q: Quat) -> [u32; 4] {
        q.to_array().map(f32::to_bits)
    }

    #[test]
    fn nlerp_normalizes_in_scalar_order() {
        let a = Quat::from_array(ADVERSARIAL);
        let b = Quat::from_array(lr_normalize(ADVERSARIAL));
        // t = 0: the linear combination is per-component exact, so the blend is exactly `a` and
        // its normalization separates glam's SIMD dot4 from the left-to-right order.
        assert_eq!(
            bits(nlerp(a, b, 0.0)),
            lr_normalize(ADVERSARIAL).map(f32::to_bits),
            "the proxy rotation must normalize in the codec's scalar order on every platform"
        );
        // A non-trivial mid-blend at t = 0.3, pinned against the same reference (the blend's
        // components are plain per-lane arithmetic, exact on every platform).
        let t = 0.3f32;
        let q = [
            a.x * (1.0 - t) + b.x * t,
            a.y * (1.0 - t) + b.y * t,
            a.z * (1.0 - t) + b.z * t,
            a.w * (1.0 - t) + b.w * t,
        ];
        assert_eq!(bits(nlerp(a, b, t)), lr_normalize(q).map(f32::to_bits));
    }

    /// Two unit quaternions whose dot straddles zero: left-to-right it is `+0.0` (no sign
    /// flip), glam's SSE2 dot4 gives `-1.49e-8` (flip). `nlerp` must choose the scalar-order
    /// sign on every platform (found by search).
    const SIGN_A: [f32; 4] = [0.467_784_14, 0.568_748_3, -0.612_140_95, 0.288_074_52];
    const SIGN_B: [f32; 4] = [0.368_422_93, 0.459_121_76, 0.369_843_42, -0.718_809_96];

    #[test]
    fn nlerp_sign_flip_uses_scalar_dot_order() {
        let a = Quat::from_array(SIGN_A);
        let b = Quat::from_array(SIGN_B);
        // The scalar dot is +0.0 (not negative), so `nlerp` keeps `b`'s sign: the blend is
        // `a * 0.5 + b * 0.5` per component. Glam's SSE2 dot4 says negative and would flip `b`,
        // so an x86_64 client would show different proxy bits than aarch64.
        let t = 0.5f32;
        let q = [
            SIGN_A[0] * (1.0 - t) + SIGN_B[0] * t,
            SIGN_A[1] * (1.0 - t) + SIGN_B[1] * t,
            SIGN_A[2] * (1.0 - t) + SIGN_B[2] * t,
            SIGN_A[3] * (1.0 - t) + SIGN_B[3] * t,
        ];
        assert_eq!(
            bits(nlerp(a, b, t)),
            lr_normalize(q).map(f32::to_bits),
            "the short-arc sign flip must follow the scalar dot order on every platform"
        );
    }

    /// corr-low (corr-net LOW): an infinite frame over an infinite half-life is a NaN ratio.
    /// It reached `pow2_neg(NaN)`, whose range `debug_assert` aborted a debug client, and a
    /// release build returned NaN, which turned the recovery offset into NaN. An undefined ratio
    /// does not decay, like `r <= 0`.
    #[test]
    fn a_nan_decay_ratio_does_not_decay() {
        assert_eq!(half_decay(f64::NAN).to_bits(), 1.0f64.to_bits());
        assert_eq!(
            half_decay(f64::INFINITY / f64::INFINITY).to_bits(),
            1.0f64.to_bits()
        );
    }

    #[test]
    fn recovery_decay_is_pinned_integer_arithmetic() {
        // The factor every recovery frame multiplies the offset by. Pinned bit-exact: a future
        // regression back to f64::powf (libm) changes the low f64 bits and, on some frames, the
        // narrowed f32 as well, so the proxy translation sequence would drift per platform.
        let d = half_decay(16.667 / 100.0);
        assert_eq!(d.to_bits(), 0x3FEC_8239_B634_EFD7, "0.5^(16.667/100)");
        assert_eq!((d as f32).to_bits(), 0x3F64_11CE, "narrowed to f32");
        assert_eq!(half_decay(0.0).to_bits(), 1.0f64.to_bits());
        assert_eq!(
            half_decay(-3.0).to_bits(),
            1.0f64.to_bits(),
            "r <= 0 does not decay"
        );
        assert_eq!(
            half_decay(2000.0).to_bits(),
            0.0f64.to_bits(),
            "far past the half-life is 0"
        );
        // Integer-branch pins (`n >= 1`, the hitch-frame path a 16.667 ms pin never reaches):
        // both factors `pow2_neg(a) * pow2_neg(b)` are exercised, so a wrong exponent in
        // `pow2_neg` for `n >= 1` fails here. Values are this implementation's exact bits; the
        // accuracy bound below keeps them honest.
        assert_eq!(half_decay(1.0).to_bits(), 0x3FE0_0000_0000_0000, "0.5^1");
        assert_eq!(half_decay(2.5).to_bits(), 0x3FC6_A09E_667F_3BF8, "0.5^2.5");
        assert_eq!(half_decay(10.3).to_bits(), 0x3F49_FDF8_BCCE_5340, "0.5^10.3");
        for r in [1.0f64, 2.5, 10.3] {
            let err = (half_decay(r) - 0.5f64.powf(r)).abs() / 0.5f64.powf(r);
            assert!(err < 1e-12, "0.5^{r} within 1e-12 relative (got {err})");
        }
        // The approximation tracks the true value closely enough for its f32 use.
        assert!((d - 0.5f64.powf(16.667 / 100.0)).abs() < 1e-14);
    }

    #[test]
    fn extrapolation_recovery_sequence_is_pinned() {
        // One extrapolated frame, a snapshot arrives, then fixed 16.667 ms frames: the shown
        // translations are pinned bit-exact so the cross-platform decay can never drift silently.
        let params = SampleParams {
            tick_hz: 60.0,
            send_interval_ticks: 2.0,
            recover_half_life_ms: 100.0,
            newest_tick: None,
        };
        let profile = InterpProfile::default();
        let state = |tick, x, v| ProxyState {
            tick,
            translation: Vec3::new(x, 0.0, 0.0),
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            velocity: v,
            teleport: false,
            steady: false,
        };
        let mut buf = SnapshotBuffer::new(8);
        buf.push(state(0, 0.0, Some(Vec3::new(1.0, 0.0, 0.0))));
        let s = buf.sample(1.0, &profile, &params, 16.667).expect("sample");
        assert_eq!(s.state, ProxyTimelineState::Extrapolating);
        assert_eq!(
            s.pose.translation.x.to_bits(),
            0x3C88_8889,
            "one tick of extrapolation"
        );
        buf.push(state(1, 0.05, None));
        let mut xs = Vec::new();
        let mut s = buf.sample(1.0, &profile, &params, 16.667).expect("sample");
        assert!(s.recovered, "new data after extrapolating starts a recovery");
        xs.push(s.pose.translation.x.to_bits());
        for _ in 0..4 {
            s = buf.sample(1.0, &profile, &params, 16.667).expect("sample");
            xs.push(s.pose.translation.x.to_bits());
        }
        assert_eq!(
            xs,
            [0x3C88_888A, 0x3CA6_536A, 0x3CC0_DE2C, 0x3CD8_8399, 0x3CED_9493,],
            "the recovery sequence is pinned across platforms"
        );
    }
}
