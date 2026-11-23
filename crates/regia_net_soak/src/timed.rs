// SPDX-License-Identifier: MPL-2.0
//! The time harness (spec 120 § 9 and § 15.4, ADR-8185): `regia_net`'s replication runtime and
//! client prediction, each process an ECS world as in `regia_net::testing::LoopbackHub`, but run
//! through the real transport ([`NetLink`] over an [`ImpairedNet`]) in virtual time with clocks
//! that drift and delays that change.
//!
//! What the lockstep hub cannot show and this harness does:
//!
//! - **Per-process clock rate.** Every process updates once per tick of its *own* clock: a client
//!   at `clock_ppm = 5_000` runs its ticks 0.5 % faster than the server, and its link clock (the
//!   milliseconds every `NetClient` call gets) runs 0.5 % fast too.
//! - **Sub-tick delivery.** Time is microseconds. Every client starts at a seeded phase inside the
//!   server's tick, and datagrams arrive at the microsecond the impairment layer gives them; a
//!   process reads what arrived by the time it updates.
//! - **RTT steps and ramps.** A link's phases ([`LinkSpec::phases`]) switch its numbers at set
//!   times; [`rtt_ramp`] builds a ramp from many small steps.
//! - **Gilbert-Elliott loss, reorder, jitter, bandwidth, outages** from [`super::impair`].
//! - **The real transport.** Reliable frames go through the transport's acknowledgments and
//!   resends (nothing is guaranteed by the harness), and the client's prediction reads its round
//!   trip from the transport's RTT estimator (`NetLink` feeds it), not from `set_transport_rtt`.
//!
//! Runs are deterministic from the seed: the impairment streams, the client phases, and the
//! update order (by due time; the server before the clients at a tie, then by client index).
//!
//! [`TimedHub::report`] gives each client's [`TimeReport`]: corrections, resets, resyncs,
//! dilation, backward clock steps, missing and late inputs, the input lead error at the server,
//! and how the transport's RTT estimate tracked the path. The harness sets no bars on them; the
//! lanes that own prediction, clock and lead (spec 120 § 17) do.

use std::net::SocketAddr;

use regia_ecs::prelude::*;
use regia_net::bridge::NetFrame;
use regia_net::capture::{CaptureRecord, CaptureTap, ReplayFeed, ReplayProgress};
use regia_net::interp::NetInterpolation;
use regia_net::link::{LinkError, net_link_flush, net_link_receive};
use regia_net::model::{LocalPeer, NetMode, NetPlayerId};
use regia_net::predict::{ClientPrediction, PredictionStats, ServerPrediction};
use regia_net::runtime::{NetNode, ensure_resources};
use regia_net::settings::NetSettings;
use regia_net::testing::NetProcess;
use regia_net::{LinkClock, LinkOptions, LinkStats, NetGame, NetLink, PeerId};
use serde::{Deserialize, Serialize};

use super::impair::{Direction, EndpointId, ImpairedNet, LinkPhase, LinkSpec, SoakRng};

/// One tick of every process's own clock at 60 Hz, microseconds (59.9988 Hz: an integer period
/// keeps the link clock and the schedule exact; every process uses the same one).
pub const TICK_US: u64 = 16_667;
/// The server's address on the impaired network.
pub const TIMED_SERVER_ADDR: ([u8; 4], u16) = ([10, 0, 0, 1], 7777);
/// Counters start this long after a client connects (the start lead settles; spec 120 § 9.2 gives
/// it 2 s), milliseconds of virtual time.
pub const DEFAULT_WARMUP_MS: u64 = 2_000;
/// The transport's RTT estimate counts as settled on a new path when it is within this many
/// milliseconds plus [`RTT_SETTLE_FRACTION`] of the truth.
pub const RTT_SETTLE_MS: f64 = 20.0;
/// See [`RTT_SETTLE_MS`].
pub const RTT_SETTLE_FRACTION: f64 = 0.1;
/// Most [`TimeEvent`]s a client keeps (the first ones; later ones are counted, not kept).
pub const MAX_TIME_EVENTS: usize = 256;

/// How a [`TimedHub`] is set up.
#[derive(Debug, Clone)]
pub struct TimedOptions {
    /// Seed of the impairment streams and the client phases.
    pub seed: u64,
    /// The server's access link (a clean datacenter link by default).
    pub server_link: LinkSpec,
    /// Counters start this long after each client connects, milliseconds.
    pub warmup_ms: u64,
    /// The server's mode (dedicated or listen).
    pub mode: NetMode,
}

impl Default for TimedOptions {
    fn default() -> Self {
        Self {
            seed: 1,
            server_link: LinkSpec::default(),
            warmup_ms: DEFAULT_WARMUP_MS,
            mode: NetMode::DedicatedServer,
        }
    }
}

/// Why a hub could not start or add a client.
#[derive(Debug)]
pub enum TimedError {
    /// The impaired network refused the address.
    Address(std::io::Error),
    /// The link refused to start.
    Link(LinkError),
}

impl std::fmt::Display for TimedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Address(e) => write!(f, "impaired address: {e}"),
            Self::Link(e) => write!(f, "link: {e}"),
        }
    }
}

impl std::error::Error for TimedError {}

impl From<std::io::Error> for TimedError {
    fn from(e: std::io::Error) -> Self {
        Self::Address(e)
    }
}

impl From<LinkError> for TimedError {
    fn from(e: LinkError) -> Self {
        Self::Link(e)
    }
}

/// Numbers of one distribution (ticks or milliseconds).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Spread {
    /// Samples taken.
    pub samples: u64,
    /// Mean of the signed values.
    pub mean: f64,
    /// Median of the signed values.
    pub p50: f64,
    /// 95th percentile of the absolute values.
    pub p95_abs: f64,
    /// Largest absolute value.
    pub max_abs: f64,
}

impl Spread {
    /// The spread of `values` (empty gives all zeros).
    pub fn of(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        let n = values.len();
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let mut abs: Vec<f64> = values.iter().map(|v| v.abs()).collect();
        abs.sort_by(f64::total_cmp);
        Self {
            samples: n as u64,
            mean: values.iter().sum::<f64>() / n as f64,
            p50: sorted[n / 2],
            p95_abs: abs[((n - 1) * 95).div_ceil(100)],
            max_abs: abs[n - 1],
        }
    }
}

/// How the transport's RTT estimate met a change of the path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RttSettle {
    /// When the path changed, milliseconds of virtual time.
    pub at_ms: u64,
    /// The path's round trip after the change (latencies plus mean jitter of both links), ms.
    pub truth_ms: f64,
    /// Milliseconds from the change until the estimate came within the settle band
    /// ([`RTT_SETTLE_MS`] + [`RTT_SETTLE_FRACTION`]); `None` if it never did before the next
    /// change or the end.
    pub settle_ms: Option<u64>,
}

/// One prediction event of a client, with the clock state around it (for finding out why).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeEvent {
    /// Virtual time, milliseconds.
    pub at_ms: u64,
    /// `correction`, `reset`, `resync` or `dilate` (the counter of `PredictionStats` that moved).
    pub kind: String,
    /// By how much that counter moved in this update (dilation: signed ticks).
    pub delta: i64,
    /// Inside the warmup (not in the report's counters).
    pub warmup: bool,
    /// The predicted tick after the update.
    pub client_tick: u32,
    /// The server's tick at that moment.
    pub server_tick: u32,
    /// The round trip prediction used (from the transport), ms.
    pub rtt_ms: Option<f64>,
    /// The jitter prediction used (from the transport), ms.
    pub jitter_ms: Option<f64>,
    /// The margin the client steers to, ticks.
    pub margin_target: f32,
    /// The margin the server last reported, ticks.
    pub reported_margin: Option<f32>,
    /// Ticks re-run by the last rollback.
    pub resim_ticks_last: u32,
}

/// One client's numbers over a run (after its warmup). The harness judges none of them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimeReport {
    /// Client index.
    pub client: usize,
    /// Its clock rate against the server's, parts per million.
    pub clock_ppm: i64,
    /// Its update phase inside the server's tick, microseconds.
    pub phase_us: u64,
    /// Milliseconds of virtual time the counters cover.
    pub measured_ms: u64,
    /// Client updates with prediction running, after warmup.
    pub predicted_ticks: u64,
    /// Server ticks after this client's warmup.
    pub server_ticks: u64,
    /// Rollbacks that resimulated (`PredictionStats::corrections`).
    pub corrections: u64,
    /// Rollbacks without history that snapped (`PredictionStats::resets`).
    pub resets: u64,
    /// Lead resyncs (`PredictionStats::resyncs`).
    pub resyncs: u64,
    /// Net predicted ticks inserted (+) or skipped (-) by dilation.
    pub dilated: i64,
    /// Client updates where the predicted tick went backward (spec 120 § 9.1 counts these).
    pub backward_steps: u64,
    /// Server ticks consumed with a repeated input because this client's was missing.
    pub missing_inputs: u64,
    /// Inputs that reached the server after their tick.
    pub late_inputs: u64,
    /// The input lead error at the server: for every update in which a newer input of this client
    /// arrived, how many ticks early it arrived minus the margin the client steers to, ticks.
    pub lead_error_ticks: Spread,
    /// Updates in which the newest input arrived after its tick had run (margin below 0).
    pub lead_late_samples: u64,
    /// The transport's RTT estimate minus the path's round trip, ms, at every client update.
    pub rtt_error_ms: Spread,
    /// How the estimate met each change of the path.
    pub rtt_settles: Vec<RttSettle>,
    /// Reliable resends of the client's transport over the run.
    pub reliable_resends: u64,
    /// Reliable resends of the server's transport to this client over the run.
    pub server_reliable_resends: u64,
    /// Prediction events from the connect on (the first [`MAX_TIME_EVENTS`]).
    pub events: Vec<TimeEvent>,
    /// Events after the first [`MAX_TIME_EVENTS`], counted only.
    pub events_dropped: u64,
    /// Times the client's link ended.
    pub disconnects: u64,
    /// The pose change of every correction and reset after warmup
    /// (`PredictionStats::last_correction_m`), meters: the own pawn's correction sizes.
    #[serde(default)]
    pub correction_m: Spread,
    /// Rollbacks whose rendered pose jumped instead of smoothing (`PredictionStats::snaps`).
    #[serde(default)]
    pub snaps: u64,
}

impl TimeReport {
    /// Corrections per predicted tick.
    pub fn correction_rate(&self) -> f64 {
        self.corrections as f64 / self.predicted_ticks.max(1) as f64
    }

    /// Missing inputs per server tick.
    pub fn missing_rate(&self) -> f64 {
        self.missing_inputs as f64 / self.server_ticks.max(1) as f64
    }

    /// One line of the numbers, for logs and lane reports.
    pub fn line(&self) -> String {
        format!(
            "client {} ppm {:+} phase {} us: {} ms, {} predicted ticks; corrections {} ({:.3} %), resets {}, resyncs {}, dilated {:+}, backward {}; missing {} ({:.3} %), late {}; lead error mean {:.2} p95 {:.2} max {:.2} ticks, late samples {}; rtt error mean {:.1} p95 {:.1} ms, settles {:?}; resends {} (server {}), disconnects {}; correction p95 {:.3} max {:.3} m ({} samples), snaps {}",
            self.client,
            self.clock_ppm,
            self.phase_us,
            self.measured_ms,
            self.predicted_ticks,
            self.corrections,
            self.correction_rate() * 100.0,
            self.resets,
            self.resyncs,
            self.dilated,
            self.backward_steps,
            self.missing_inputs,
            self.missing_rate() * 100.0,
            self.late_inputs,
            self.lead_error_ticks.mean,
            self.lead_error_ticks.p95_abs,
            self.lead_error_ticks.max_abs,
            self.lead_late_samples,
            self.rtt_error_ms.mean,
            self.rtt_error_ms.p95_abs,
            self.rtt_settles
                .iter()
                .map(|s| (s.at_ms, s.settle_ms))
                .collect::<Vec<_>>(),
            self.reliable_resends,
            self.server_reliable_resends,
            self.disconnects,
            self.correction_m.p95_abs,
            self.correction_m.max_abs,
            self.correction_m.samples,
            self.snaps,
        )
    }
}

/// The counters a report subtracts (taken when the warmup ends).
#[derive(Debug, Clone, Default)]
struct Baseline {
    at_us: u64,
    stats: PredictionStats,
    missing: u64,
    late: u64,
    server_ticks: u64,
}

#[derive(Debug, Default)]
struct Track {
    connected_at_us: Option<u64>,
    baseline: Option<Baseline>,
    last_tick: Option<u32>,
    backward_steps: u64,
    predicted_ticks: u64,
    lead_errors: Vec<f64>,
    lead_late: u64,
    rtt_errors: Vec<f64>,
    /// `(virtual ms, estimate ms, truth ms)` at every client update after warmup.
    rtt_series: Vec<(u64, f64, f64)>,
    was_connected: bool,
    disconnects: u64,
    /// `PredictionStats::last_correction_m` of each correction or reset after warmup.
    correction_sizes: Vec<f64>,
    /// `(ms since the connect, arrival margin, margin target)` per lead error sample.
    lead_series: Vec<(u64, f64, f64)>,
    last_stats: Option<PredictionStats>,
    events: Vec<TimeEvent>,
    events_dropped: u64,
}

/// One process of the hub: its world and schedule, its endpoint and its clock.
struct Member {
    proc: NetProcess,
    ep: EndpointId,
    spec: LinkSpec,
    phase_us: u64,
    updates: u64,
    track: Track,
    /// The frame digest a captured client writes after each update, and its chain so far.
    digest: Option<(FrameDigest, u64)>,
}

/// A digest of what a client world shows and predicts after an update (its proxies' interpolated
/// state, its predicted state, its correction counters): [`TimedHub::capture_client`] chains it
/// into the capture's frame markers, and [`replay_client`] computes the same on the replay.
pub type FrameDigest = fn(&mut World) -> u64;

/// Chains one frame's digest onto `chain` (FNV-1a over the two words).
fn chain_digest(chain: u64, frame: u64) -> u64 {
    let mut h = super::workload::Fnv64::new(chain);
    h.u64(frame);
    h.finish()
}

impl Member {
    /// Virtual time of update `k`: the phase plus `k` ticks of this process's clock.
    fn due_us(&self, k: u64) -> u64 {
        let ppm = i128::from(self.spec.clock_ppm);
        let scaled = i128::from(k) * i128::from(TICK_US) * 1_000_000 / (1_000_000 + ppm).max(1);
        self.phase_us + scaled.clamp(0, i128::from(u64::MAX)) as u64
    }

    fn next_us(&self) -> u64 {
        self.due_us(self.updates)
    }

    fn update(&mut self) {
        let world = &mut self.proc.world;
        net_link_receive(world);
        // `LinkInterpolationPlugin`'s arrival stamp, on this process's link clock.
        if let Some(now) = world.get_resource::<NetLink>().map(|l| l.now_ms() as f64)
            && let Some(mut ni) = world.get_resource_mut::<NetInterpolation>()
        {
            ni.now_ms = now;
        }
        self.proc.tick();
        net_link_flush(&mut self.proc.world);
        self.updates += 1;
        if let Some((digest, chain)) = self.digest.as_mut() {
            *chain = chain_digest(*chain, digest(&mut self.proc.world));
            if let Some(link) = self.proc.world.get_resource::<NetLink>() {
                link.capture_frame(*chain);
            }
        }
    }
}

fn new_world(mode: NetMode, setup: fn(&mut World)) -> World {
    let mut world = World::new();
    world.insert_resource(mode);
    world.insert_resource(LocalPeer(None));
    world.insert_resource(NetSettings::default());
    setup(&mut world);
    ensure_resources(&mut world);
    world.insert_resource(NetLink::default());
    world
}

fn link_options(start_ms: u64) -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms,
            step_us: TICK_US,
        },
        tick_rate_hz: 60,
        ..LinkOptions::default()
    }
}

/// The round trip a datagram pair sees on `client` and `server` at `now_ms`: both links' latencies
/// both ways plus their mean jitter, milliseconds.
pub fn path_rtt_ms(client: &LinkSpec, server: &LinkSpec, now_ms: u64) -> f64 {
    let (cu, cd) = client.directions_at(now_ms);
    let (su, sd) = server.directions_at(now_ms);
    [cu, cd, su, sd]
        .iter()
        .map(|d| d.latency_ms as f64 + d.jitter_ms as f64 / 2.0)
        .sum()
}

/// Phases that move both directions' one-way latency of `base` from `from_ms` to `to_ms` in
/// `steps` equal steps over `[start_ms, start_ms + len_ms]` (a route that slowly fills: a ramp).
/// The other numbers of each direction stay as in `base`.
pub fn rtt_ramp(
    base: &LinkSpec,
    from_ms: u64,
    to_ms: u64,
    start_ms: u64,
    len_ms: u64,
    steps: u32,
) -> Vec<LinkPhase> {
    let steps = steps.max(1);
    (1..=steps)
        .map(|i| {
            let f = i64::from(i) * (to_ms as i64 - from_ms as i64) / i64::from(steps);
            let latency_ms = (from_ms as i64 + f).max(0) as u64;
            let up = Direction {
                latency_ms,
                ..base.up.clone()
            };
            let down = Direction {
                latency_ms,
                ..base.down.clone()
            };
            LinkPhase {
                at_ms: start_ms + len_ms * u64::from(i) / u64::from(steps),
                up,
                down,
            }
        })
        .collect()
}

/// The server and clients as ECS worlds on one impaired network, in virtual time (module docs).
pub struct TimedHub {
    net: ImpairedNet,
    server: Member,
    clients: Vec<Member>,
    setup: fn(&mut World),
    options: TimedOptions,
    rng: SoakRng,
    now_us: u64,
}

impl std::fmt::Debug for TimedHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimedHub")
            .field("now_us", &self.now_us)
            .field("clients", &self.clients.len())
            .finish()
    }
}

impl TimedHub {
    /// A hub whose server hosts game `G`. `setup` runs on every world before the link starts
    /// (register messages, components and prediction there, in the same order everywhere); every
    /// world starts from `NetSettings::default()`, so a test that wants the lockstep hub's
    /// settings sets them in `setup`.
    ///
    /// # Errors
    ///
    /// The server's address or link could not start.
    pub fn new<G: NetGame<Message = NetFrame>>(
        options: TimedOptions,
        setup: fn(&mut World),
    ) -> Result<Self, TimedError> {
        Self::with_max_peers::<G>(options, setup, LinkOptions::default().max_peers)
    }

    /// [`Self::new`] with the most clients the server accepts (`LinkOptions::max_peers`; `new`
    /// keeps its default of 16).
    ///
    /// # Errors
    ///
    /// The server's address or link could not start.
    pub fn with_max_peers<G: NetGame<Message = NetFrame>>(
        options: TimedOptions,
        setup: fn(&mut World),
        max_peers: usize,
    ) -> Result<Self, TimedError> {
        let net = ImpairedNet::new(options.seed);
        let addr = SocketAddr::from(TIMED_SERVER_ADDR);
        let ep = net.add_endpoint(addr, options.server_link.clone())?;
        let mut world = new_world(options.mode, setup);
        world.insert_resource(LocalPeer(Some(PeerId::SERVER)));
        world.resource_mut::<NetLink>().host::<G>(
            Box::new(net.socket(ep)),
            &LinkOptions {
                max_peers,
                ..link_options(0)
            },
        )?;
        let server = Member {
            proc: NetProcess {
                world,
                gameplay: Schedule::default(),
                peer: PeerId::SERVER,
                connected: true,
            },
            ep,
            spec: options.server_link.clone(),
            phase_us: 0,
            updates: 0,
            track: Track::default(),
            digest: None,
        };
        Ok(Self {
            net,
            server,
            clients: Vec::new(),
            setup,
            rng: SoakRng::derive(options.seed, 0x7135),
            options,
            now_us: 0,
        })
    }

    /// Adds a client on access link `link` (its `clock_ppm` sets its clock rate) that connects
    /// now, at a seeded phase inside the tick; returns its index.
    ///
    /// # Errors
    ///
    /// The address or the link could not start.
    pub fn add_client<G: NetGame<Message = NetFrame, JoinData = ()>>(
        &mut self,
        link: LinkSpec,
    ) -> Result<usize, TimedError> {
        let phase = self.now_us + 1 + self.rng.up_to(TICK_US - 2);
        self.add_client_at::<G>(link, phase)
    }

    /// [`Self::add_client`] with its first update at virtual time `first_update_us` (at least now).
    ///
    /// # Errors
    ///
    /// The address or the link could not start.
    pub fn add_client_at<G: NetGame<Message = NetFrame, JoinData = ()>>(
        &mut self,
        link: LinkSpec,
        first_update_us: u64,
    ) -> Result<usize, TimedError> {
        let i = self.clients.len();
        let addr = SocketAddr::from(([10, 1, (i / 250) as u8, (i % 250) as u8 + 2], 40_000));
        let ep = self.net.add_endpoint(addr, link.clone())?;
        let mut world = new_world(NetMode::Client, self.setup);
        // A client's local clock starts at a seeded reading: nothing may assume it matches the
        // server's.
        let start_ms = 1_000 + self.rng.up_to(60_000);
        world.resource_mut::<NetLink>().connect::<G>(
            Box::new(self.net.socket(ep)),
            SocketAddr::from(TIMED_SERVER_ADDR),
            &link_options(start_ms),
        )?;
        self.clients.push(Member {
            proc: NetProcess {
                world,
                gameplay: Schedule::default(),
                peer: PeerId::SERVER,
                connected: false,
            },
            ep,
            spec: link,
            phase_us: first_update_us.max(self.now_us),
            updates: 0,
            track: Track::default(),
            digest: None,
        });
        Ok(i)
    }

    /// Captures client `i` through its [`NetLink`] (ADR-8890): a ring of `ring_bytes` gets every
    /// packet it receives, its receive and flush times and its end, and after each of its updates
    /// a frame marker with `digest` chained over the updates. Call it right after the client was
    /// added (before its first update) to capture the whole connection.
    ///
    /// # Errors
    ///
    /// `capture_config_invalid` for a ring size out of range (as a [`TimedError::Link`]).
    pub fn capture_client(
        &mut self,
        i: usize,
        ring_bytes: usize,
        digest: FrameDigest,
    ) -> Result<CaptureTap, TimedError> {
        let tap = CaptureTap::new(ring_bytes).map_err(LinkError::Capture)?;
        let c = &mut self.clients[i];
        c.proc
            .world
            .resource_mut::<NetLink>()
            .set_capture(Some(tap.clone()));
        c.digest = Some((digest, 0));
        Ok(tap)
    }

    /// Client `i`'s chained frame digest so far (`None` when it is not captured).
    pub fn client_digest(&self, i: usize) -> Option<u64> {
        self.clients[i].digest.map(|(_, chain)| chain)
    }

    /// The impairment network (counters per endpoint).
    pub fn net(&self) -> &ImpairedNet {
        &self.net
    }

    /// Virtual time, microseconds.
    pub fn now_us(&self) -> u64 {
        self.now_us
    }

    /// The server process (its world and gameplay schedule).
    pub fn server(&mut self) -> &mut NetProcess {
        &mut self.server.proc
    }

    /// The server world.
    pub fn server_world(&self) -> &World {
        &self.server.proc.world
    }

    /// Client `i`'s process.
    pub fn client(&mut self, i: usize) -> &mut NetProcess {
        &mut self.clients[i].proc
    }

    /// Client `i`'s world.
    pub fn client_world(&self, i: usize) -> &World {
        &self.clients[i].proc.world
    }

    /// Number of clients.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Client `i`'s endpoint on the impaired network.
    pub fn client_endpoint(&self, i: usize) -> EndpointId {
        self.clients[i].ep
    }

    /// The peer id the server gave client `i` (`None` before it connected).
    pub fn client_peer(&self, i: usize) -> Option<PeerId> {
        let w = &self.clients[i].proc.world;
        w.get_resource::<NetLink>()
            .filter(|l| l.is_connected())
            .and(w.get_resource::<LocalPeer>().and_then(|p| p.0))
    }

    /// The server's current tick.
    pub fn server_tick(&self) -> u32 {
        self.server.proc.world.resource::<NetNode>().tick.0
    }

    /// Updates the server has run.
    pub fn server_updates(&self) -> u64 {
        self.server.updates
    }

    /// Updates client `i` has run.
    pub fn client_updates(&self, i: usize) -> u64 {
        self.clients[i].updates
    }

    /// Client `i`'s transport link counters (`None` before it connected).
    pub fn client_link_stats(&self, i: usize) -> Option<LinkStats> {
        self.clients[i]
            .proc
            .world
            .get_resource::<NetLink>()
            .and_then(NetLink::status)
            .and_then(|s| s.link)
    }

    /// The server transport's link counters for client `i` (from the server's endpoint report;
    /// `None` before it joined).
    pub fn server_link_stats(&self, i: usize) -> Option<LinkStats> {
        let peer = self.client_peer(i)?;
        let world = &self.server.proc.world;
        let label = world.get_resource::<NetLink>()?.label().to_owned();
        let label = if label.is_empty() {
            "server".to_owned()
        } else {
            label
        };
        world
            .get_resource::<regia_net::NetAgentState>()?
            .reports()
            .get(&label)?
            .peers
            .iter()
            .find(|p| p.id == peer.0)
            .map(|p| p.link)
    }

    /// Runs the next due update (the earliest; the server first at a tie, then by client index).
    pub fn step(&mut self) {
        let mut next = (self.server.next_us(), 0usize);
        for (i, c) in self.clients.iter().enumerate() {
            let t = c.next_us();
            if t < next.0 {
                next = (t, i + 1);
            }
        }
        let (at, who) = next;
        self.now_us = self.now_us.max(at);
        self.net.advance_to_us(self.now_us);
        if who == 0 {
            self.update_server();
        } else {
            self.update_client(who - 1);
        }
    }

    /// Runs every update due up to virtual time `t_us`.
    pub fn run_until_us(&mut self, t_us: u64) {
        while self.next_due_us() <= t_us {
            self.step();
        }
        self.now_us = self.now_us.max(t_us);
        self.net.advance_to_us(self.now_us);
    }

    /// Runs `ms` milliseconds of virtual time.
    pub fn run_ms(&mut self, ms: u64) {
        self.run_until_us(self.now_us + ms * 1000);
    }

    /// Runs until `done` holds or `limit_ms` of virtual time passed; true when it held.
    pub fn run_until(&mut self, limit_ms: u64, mut done: impl FnMut(&Self) -> bool) -> bool {
        let end = self.now_us + limit_ms * 1000;
        while !done(self) {
            if self.next_due_us() > end {
                return false;
            }
            self.step();
        }
        true
    }

    fn next_due_us(&self) -> u64 {
        self.clients
            .iter()
            .map(Member::next_us)
            .fold(self.server.next_us(), u64::min)
    }

    fn update_server(&mut self) {
        let before_tick = self.server_tick();
        let players: Vec<(usize, NetPlayerId, Option<u32>)> = self
            .clients
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                c.track.baseline.as_ref()?;
                let peer = c.proc.world.get_resource::<LocalPeer>()?.0?;
                let player = NetPlayerId { peer, local: 0 };
                Some((i, player, self.newest_input(player)))
            })
            .collect();
        self.server.update();
        for (i, player, newest_before) in players {
            let newest = self.newest_input(player);
            let c = &mut self.clients[i];
            if newest.is_some() && newest != newest_before {
                let margin = f64::from(newest.unwrap_or(0)) - f64::from(before_tick);
                let world = &c.proc.world;
                let target = world.get_resource::<ClientPrediction>().map_or(0.0, |cp| {
                    f64::from(cp.margin_target(&world.resource::<NetSettings>().prediction))
                });
                c.track.lead_errors.push(margin - target);
                let since = c
                    .track
                    .connected_at_us
                    .map_or(0, |at| self.now_us.saturating_sub(at) / 1000);
                c.track.lead_series.push((since, margin, target));
                if margin < 0.0 {
                    c.track.lead_late += 1;
                }
            }
        }
    }

    fn newest_input(&self, player: NetPlayerId) -> Option<u32> {
        self.server
            .proc
            .world
            .get_resource::<ServerPrediction>()
            .and_then(|sp| sp.players.get(&player))
            .and_then(|p| p.newest_tick)
            .map(|t| t.0)
    }

    fn update_client(&mut self, i: usize) {
        let now_us = self.now_us;
        let warmup_us = self.options.warmup_ms * 1000;
        let server_ticks = self.server.updates;
        let server_tick = self.server_tick();
        let server_spec = self.server.spec.clone();
        let (missing, late) = self.client_peer(i).map_or((0, 0), |peer| {
            self.server
                .proc
                .world
                .get_resource::<ServerPrediction>()
                .and_then(|sp| sp.players.get(&NetPlayerId { peer, local: 0 }))
                .map_or((0, 0), |p| (p.missing, p.late))
        });
        let c = &mut self.clients[i];
        c.update();
        let connected = c
            .proc
            .world
            .get_resource::<NetLink>()
            .is_some_and(NetLink::is_connected);
        c.proc.connected = connected;
        if let Some(peer) = c.proc.world.get_resource::<LocalPeer>().and_then(|p| p.0) {
            c.proc.peer = peer;
        }
        let t = &mut c.track;
        if connected && t.connected_at_us.is_none() {
            t.connected_at_us = Some(now_us);
        }
        if t.was_connected && !connected {
            t.disconnects += 1;
        }
        t.was_connected = connected;
        let Some(at) = t.connected_at_us else {
            return;
        };
        let cp = c.proc.world.get_resource::<ClientPrediction>();
        if let Some(cp) = cp {
            let settings = &c.proc.world.resource::<NetSettings>().prediction;
            let s = &cp.stats;
            let last = t.last_stats.take().unwrap_or_default();
            let moved = [
                ("correction", s.corrections as i64 - last.corrections as i64),
                ("reset", s.resets as i64 - last.resets as i64),
                ("resync", s.resyncs as i64 - last.resyncs as i64),
                ("dilate", s.dilated - last.dilated),
            ];
            if t.baseline.is_some() && (moved[0].1 > 0 || moved[1].1 > 0) {
                t.correction_sizes.push(f64::from(s.last_correction_m));
            }
            for (kind, delta) in moved.into_iter().filter(|(_, d)| *d != 0) {
                if t.events.len() >= MAX_TIME_EVENTS {
                    t.events_dropped += 1;
                    continue;
                }
                t.events.push(TimeEvent {
                    at_ms: now_us / 1000,
                    kind: kind.to_owned(),
                    delta,
                    warmup: t.baseline.is_none(),
                    client_tick: cp.client_tick().0,
                    server_tick,
                    rtt_ms: cp.transport_rtt.map(|r| r.0),
                    jitter_ms: cp.transport_rtt.map(|r| r.1),
                    margin_target: cp.margin_target(settings),
                    reported_margin: cp.reported_margin(),
                    resim_ticks_last: s.resim_ticks_last,
                });
            }
            t.last_stats = Some(s.clone());
        }
        if t.baseline.is_none() && now_us >= at + warmup_us {
            t.baseline = Some(Baseline {
                at_us: now_us,
                stats: cp.map(|cp| cp.stats.clone()).unwrap_or_default(),
                missing,
                late,
                server_ticks,
            });
        }
        if t.baseline.is_none() {
            return;
        }
        if let Some(cp) = cp.filter(|cp| cp.started) {
            t.predicted_ticks += 1;
            let tick = cp.client_tick().0;
            if t.last_tick.is_some_and(|last| tick < last) {
                t.backward_steps += 1;
            }
            t.last_tick = Some(tick);
        }
        if let Some(rtt) = c
            .proc
            .world
            .get_resource::<NetLink>()
            .and_then(NetLink::status)
            .and_then(|s| s.link)
            .map(|l| l.rtt_ms)
            .filter(|r| *r > 0.0)
        {
            let now_ms = now_us / 1000;
            let truth = path_rtt_ms(&c.spec, &server_spec, now_ms);
            t.rtt_errors.push(rtt - truth);
            t.rtt_series.push((now_ms, rtt, truth));
        }
    }

    /// Client `i`'s numbers from the end of its warmup to now.
    pub fn report(&self, i: usize) -> TimeReport {
        let c = &self.clients[i];
        let t = &c.track;
        let mut r = TimeReport {
            client: i,
            clock_ppm: c.spec.clock_ppm,
            phase_us: c.phase_us,
            predicted_ticks: t.predicted_ticks,
            backward_steps: t.backward_steps,
            lead_error_ticks: Spread::of(&t.lead_errors),
            lead_late_samples: t.lead_late,
            rtt_error_ms: Spread::of(&t.rtt_errors),
            disconnects: t.disconnects,
            reliable_resends: self.client_link_stats(i).map_or(0, |l| l.reliable_resends),
            server_reliable_resends: self.server_link_stats(i).map_or(0, |l| l.reliable_resends),
            events: t.events.clone(),
            events_dropped: t.events_dropped,
            correction_m: Spread::of(&t.correction_sizes),
            ..TimeReport::default()
        };
        let Some(b) = &t.baseline else {
            return r;
        };
        r.measured_ms = self.now_us.saturating_sub(b.at_us) / 1000;
        r.server_ticks = self.server.updates.saturating_sub(b.server_ticks);
        if let Some(cp) = c.proc.world.get_resource::<ClientPrediction>() {
            let s = &cp.stats;
            r.corrections = s.corrections.saturating_sub(b.stats.corrections);
            r.resets = s.resets.saturating_sub(b.stats.resets);
            r.resyncs = s.resyncs.saturating_sub(b.stats.resyncs);
            r.dilated = s.dilated - b.stats.dilated;
            r.snaps = s.snaps.saturating_sub(b.stats.snaps);
        }
        if let Some(peer) = self.client_peer(i)
            && let Some(p) = self
                .server
                .proc
                .world
                .get_resource::<ServerPrediction>()
                .and_then(|sp| sp.players.get(&NetPlayerId { peer, local: 0 }))
        {
            r.missing_inputs = p.missing.saturating_sub(b.missing);
            r.late_inputs = p.late.saturating_sub(b.late);
        }
        r.rtt_settles = settles(&c.spec, &self.server.spec, &t.rtt_series, b.at_us / 1000);
        r
    }

    /// Client `i`'s input lead after warmup, one row per server update in which a newer input of it
    /// arrived: `(milliseconds since its connect, arrival margin in ticks, the margin target it
    /// steered to then)`.
    pub fn lead_samples(&self, i: usize) -> &[(u64, f64, f64)] {
        &self.clients[i].track.lead_series
    }

    /// Every client's report.
    pub fn reports(&self) -> Vec<TimeReport> {
        (0..self.clients.len()).map(|i| self.report(i)).collect()
    }
}

/// For every phase change of either link after `from_ms`: how long the estimate took to come
/// within the settle band of the new round trip.
/// What [`replay_client`] gave.
#[derive(Debug, Clone)]
pub struct TimedReplay {
    /// Updates the replay ran (one per captured receive).
    pub updates: u64,
    /// The replay's chained frame digest.
    pub digest: u64,
    /// The captured chained frame digest (the session's last frame marker).
    pub captured_digest: Option<u64>,
    /// The records the replay's own capture wrote (one session).
    pub records: Vec<CaptureRecord>,
    /// How far the captured session got.
    pub progress: Option<ReplayProgress>,
    /// The replayed world's prediction counters (`None` without client prediction).
    pub prediction: Option<PredictionStats>,
    /// The first record where the replay differs from the capture (`None` when it matches).
    pub divergence: Option<regia_net::capture::CaptureDivergence>,
}

impl TimedReplay {
    /// True when the replay wrote the captured times, inputs, end and frame digests exactly.
    pub fn matches(&self) -> bool {
        self.divergence.is_none() && self.captured_digest == Some(self.digest)
    }
}

/// Replays one captured client session of a [`TimedHub`] (ADR-8890) through [`NetLink::replay`]:
/// a fresh client world made with `setup` (and `configure`, which adds what the hub's test added
/// to the client: its gameplay systems and resources) reads the captured packets at the captured
/// link-clock times, one update per captured receive, and writes `digest` chained after each
/// update, as [`TimedHub::capture_client`] did. Its own capture is compared with `session`.
///
/// # Errors
///
/// The session is empty or of another game ([`TimedError::Link`] with the capture's code).
pub fn replay_client<G: NetGame<Message = NetFrame, JoinData = ()>>(
    session: &[CaptureRecord],
    setup: fn(&mut World),
    configure: impl FnOnce(&mut NetProcess),
    digest: FrameDigest,
) -> Result<TimedReplay, TimedError> {
    let receives: Vec<u64> = session
        .iter()
        .filter_map(|r| match r {
            CaptureRecord::Receive { at_ms } => Some(*at_ms),
            _ => None,
        })
        .collect();
    let captured_digest = session.iter().rev().find_map(|r| match r {
        CaptureRecord::Frame { digest, .. } => Some(*digest),
        _ => None,
    });
    let start_ms = receives.first().copied().unwrap_or(0);
    let feed = ReplayFeed::new(session.to_vec()).map_err(LinkError::Capture)?;
    let need: usize = session.iter().map(CaptureRecord::ring_bytes).sum();
    let ring = (need + need / 4 + (64 << 10)).clamp(
        regia_net::capture::MIN_CAPTURE_RING_BYTES,
        regia_net::capture::MAX_CAPTURE_RING_BYTES,
    );
    let tap = CaptureTap::new(ring).map_err(LinkError::Capture)?;
    let mut world = new_world(NetMode::Client, setup);
    {
        let mut link = world.resource_mut::<NetLink>();
        link.replay::<G>(feed, &link_options(start_ms))?;
        link.set_capture(Some(tap.clone()));
    }
    let mut member = Member {
        proc: NetProcess {
            world,
            gameplay: Schedule::default(),
            peer: PeerId::SERVER,
            connected: false,
        },
        // A replay has no endpoint on any network.
        ep: EndpointId(usize::MAX),
        spec: LinkSpec::default(),
        phase_us: 0,
        updates: 0,
        track: Track::default(),
        digest: Some((digest, 0)),
    };
    configure(&mut member.proc);
    for _ in &receives {
        member.update();
        let world = &member.proc.world;
        member.proc.connected = world.get_resource::<NetLink>().is_some_and(NetLink::is_connected);
        if let Some(peer) = world.get_resource::<LocalPeer>().and_then(|p| p.0) {
            member.proc.peer = peer;
        }
    }
    let world = &member.proc.world;
    let link = world.resource::<NetLink>();
    let records = tap.lock().sessions().into_iter().next().unwrap_or_default();
    let divergence = regia_net::capture::compare_sessions(session, &records);
    Ok(TimedReplay {
        updates: member.updates,
        digest: member.digest.map_or(0, |(_, chain)| chain),
        captured_digest,
        progress: link.replay_progress(),
        prediction: world
            .get_resource::<ClientPrediction>()
            .map(|cp| cp.stats.clone()),
        divergence,
        records,
    })
}

fn settles(client: &LinkSpec, server: &LinkSpec, series: &[(u64, f64, f64)], from_ms: u64) -> Vec<RttSettle> {
    let mut changes: Vec<u64> = client
        .phases
        .iter()
        .chain(server.phases.iter())
        .map(|p| p.at_ms)
        .filter(|&at| at >= from_ms)
        .collect();
    changes.sort_unstable();
    changes.dedup();
    let end = series.last().map_or(0, |s| s.0);
    changes
        .iter()
        .enumerate()
        .filter(|&(_, &at)| at <= end)
        .map(|(k, &at)| {
            let until = changes.get(k + 1).copied().unwrap_or(u64::MAX);
            let truth = path_rtt_ms(client, server, at);
            let settle_ms = series
                .iter()
                .filter(|s| s.0 >= at && s.0 < until)
                .find(|s| (s.1 - s.2).abs() <= RTT_SETTLE_MS + RTT_SETTLE_FRACTION * s.2)
                .map(|s| s.0 - at);
            RttSettle {
                at_ms: at,
                truth_ms: truth,
                settle_ms,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fast_clock_updates_more_often() {
        let base = Member {
            proc: NetProcess {
                world: World::new(),
                gameplay: Schedule::default(),
                peer: PeerId::SERVER,
                connected: false,
            },
            ep: EndpointId(0),
            spec: LinkSpec::default(),
            phase_us: 500,
            updates: 0,
            track: Track::default(),
            digest: None,
        };
        assert_eq!(base.due_us(0), 500);
        assert_eq!(base.due_us(3_600), 500 + 3_600 * TICK_US);
        let fast = Member {
            spec: LinkSpec {
                clock_ppm: 5_000,
                ..LinkSpec::default()
            },
            ..base
        };
        // 0.5 % fast: 3,618 own ticks fit where the server runs 3,600.
        let minute = 3_600 * TICK_US + 500;
        let fast_updates = (0..4_000).take_while(|&k| fast.due_us(k) <= minute).count();
        assert_eq!(fast_updates, 3_619, "3,618 ticks plus the one at the phase");
    }

    #[test]
    fn a_ramp_steps_latency_evenly() {
        let base = LinkSpec {
            up: Direction {
                latency_ms: 20,
                jitter_ms: 5,
                ..Direction::clean()
            },
            down: Direction {
                latency_ms: 20,
                ..Direction::clean()
            },
            ..LinkSpec::default()
        };
        let ramp = rtt_ramp(&base, 20, 120, 10_000, 10_000, 4);
        let lat: Vec<(u64, u64, u64)> = ramp
            .iter()
            .map(|p| (p.at_ms, p.up.latency_ms, p.down.latency_ms))
            .collect();
        assert_eq!(
            lat,
            vec![
                (12_500, 45, 45),
                (15_000, 70, 70),
                (17_500, 95, 95),
                (20_000, 120, 120)
            ]
        );
        assert!(ramp.iter().all(|p| p.up.jitter_ms == 5), "other numbers kept");
        let spec = LinkSpec { phases: ramp, ..base };
        assert_eq!(path_rtt_ms(&spec, &LinkSpec::default(), 0), 42.5);
        assert_eq!(path_rtt_ms(&spec, &LinkSpec::default(), 16_000), 142.5);
    }

    #[test]
    fn spread_reports_signed_center_and_absolute_tail() {
        let s = Spread::of(&[-3.0, 1.0, 1.0, 2.0]);
        assert_eq!(s.samples, 4);
        assert_eq!(s.mean, 0.25);
        assert_eq!(s.p50, 1.0);
        assert_eq!(s.max_abs, 3.0);
        assert_eq!(s.p95_abs, 3.0);
        assert_eq!(Spread::of(&[]), Spread::default());
    }

    #[test]
    fn settles_measure_each_change() {
        let base = LinkSpec {
            up: Direction {
                latency_ms: 20,
                ..Direction::clean()
            },
            down: Direction {
                latency_ms: 20,
                ..Direction::clean()
            },
            ..LinkSpec::default()
        };
        let phases = rtt_ramp(&base, 20, 125, 1_000, 0, 1);
        let spec = LinkSpec { phases, ..base };
        // Estimate 40 until 1.5 s, then 250.
        let series: Vec<(u64, f64, f64)> = (0..30)
            .map(|k| {
                let t = k * 100;
                let est = if t < 1_500 { 40.0 } else { 250.0 };
                (t, est, path_rtt_ms(&spec, &LinkSpec::default(), t))
            })
            .collect();
        let s = settles(&spec, &LinkSpec::default(), &series, 0);
        assert_eq!(
            s,
            vec![RttSettle {
                at_ms: 1_000,
                truth_ms: 250.0,
                settle_ms: Some(500)
            }]
        );
    }
}
