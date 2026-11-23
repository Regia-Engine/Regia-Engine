// SPDX-License-Identifier: MPL-2.0
//! Measurements, bars and the `result.json` / `summary.md` files of a soak (ADR-7899).
//!
//! The bars (also in `docs/06-testing-and-ci.md` § net-soak and `book/agents/net-soak.md`):
//!
//! | Bar | Rule |
//! |---|---|
//! | `involuntary_disconnects` | No connection lost that the workload did not end (one per connection: the client's end, or the server's when the client never noticed), unless it is the engine's own removal: inside an outage longer than the removal timeout ([`timeout_policy`]) and not more than [`REMOVAL_EARLY_MS`] before that timeout has run (`flappy` and `long-outage` have such outages) |
//! | `reconnect_within_grace` | Every rejoin is connected within [`RECONNECT_GRACE_MS`] of its link being usable |
//! | `no_wedged_client` | Every client joins; no connected client goes [`WEDGE_MS`] with its link up (an outage a gap overlaps excuses itself and [`INPUT_GRACE_MS`] after it) and no input reaching the server (until the client's leave or the server's removal, ADR-8887), no snapshot arriving, or every input send failing |
//! | `hostile_dropped` | (`hostile`) every hostile session is dropped by the server within [`HOSTILE_DROP_MS`] of its first hostile act |
//! | `others_unaffected` | (`hostile`) no honest client has an involuntary end or a wedge while the hostile peer is active, or snapshot staleness more than [`HOSTILE_STALENESS_TICKS`] above its own worst before the hostile peer started (seconds inside its own outages and 1 s after them left out) |
//! | `bandwidth_within_budget` | (`congested`) every client's offered downstream rate over the second half of the run is at most the link's cap |
//!
//! Spec 120 § 15.2 adds fifteen bars (ADR-8030), each a function of `result.json` ([`spec120_bars`]
//! of a [`ScenarioResult`]), reported on every run whose profile they name and **not gating** until
//! the lane that owns each flips it (`xtask/src/net_soak.rs` `BARS`; the gate's list reaches this
//! crate as `REGIA_NET_SOAK_GATING`, else [`GATING_BARS`]; `lockstep_upload` gates since ADR-8209,
//! `interrupted_once` and `resume_same_peer` since ADR-10093):
//!
//! | Bar | Pass when | Profiles |
//! |---|---|---|
//! | `missed_input` | the run's missed input ticks <= 0.5 % (clean, good-wifi), 2 % (bad-wifi, mobile-4g), 3 % (asym-bufferbloat) | those |
//! | `corrections` | corrections <= 1 % of compared predictions (good-wifi), 3 % (bad-wifi) | those |
//! | `staleness_p99` | snapshot staleness p99 with the link up <= 4 ticks (<= 8 for congested clients) | every run |
//! | `rate_vs_bottleneck` | from 5 s after each connect, every whole second's offered wire rate at every finite bottleneck <= its rate | congested, bufferbloat, listen-uplink |
//! | `queue_delay_p95` | the game's datagrams wait <= 100 ms p95 at the bottleneck queues | bufferbloat |
//! | `rpc_p95_with_bulk` | command round trip p95 while the bulk transfer is in flight <= 1.5x the same run's p95 before it | `+bulk` runs |
//! | `random_loss_kept` | the smallest snapshot entity budget after 5 s, outside a first flight (ADR-10200), >= 90 % of the full world (a clean run's budget) | lossy-fat-pipe |
//! | `interrupted_once` | one `Interrupted` and one `Restored` per outage of at least the engine's `interrupt_ms` ([`timeout_policy`]) while connected, none elsewhere; an outage beyond the removal timeout ends with the `Restored` of the automatic reconnect (ADR-8352), and one the client left on purpose inside owes none (ADR-10093) | flappy, long-outage, good-wifi |
//! | `resume_same_peer` | every involuntary rejoin gets its old `PeerId` back (and so its avatar) | long-outage, wifi-to-cell |
//! | `no_hijack` | the server never moves a peer to an address its client did not hold | hostile, nat-rebind, cgnat |
//! | `never_kicked_for_ping` | every rtt-extreme peer joins and is never disconnected | rtt-extreme |
//! | `desync_zero` | 0 desync verdicts, 0 state transfers (seats and host), no seat removed, the votes decided, every simulation in step at the last common tick, and at least one checkpoint agreed | lockstep-mix, lockstep-mix+action, lockstep-mix+bots |
//! | `lockstep_upload` | the host's upload, IP and UDP headers included: the 95th-percentile whole second <= [`UPLOAD_P95_LIMIT_BPS`] and the busiest second <= [`UPLOAD_PEAK_LIMIT_BPS`] (stated over a 5-minute run; a shorter custom run is judged on its seconds), at up to [`LOCKSTEP_UPLOAD_SEATS_ORDERS`] seats on the orders mix and the bots mix and [`LOCKSTEP_UPLOAD_SEATS_ACTION`] on the action mix (a run with more seats reports the numbers and skips the bar) | lockstep-mix, lockstep-mix+action, lockstep-mix+bots |
//! | `tick_p99` | server tick wall time p99 <= the tick period ([`TICK_P99_BUDGET_US`]) | mixed |
//! | `join_under_flood` | >= 99 % of join attempts connected within 5 s | forged-handshake |
//!
//! A bar a v1 `result.json` has no numbers for is reported skipped (`not measured in a v1 result`).
//!
//! A `lockstep-mix` run is a lockstep match, not a replication run (ADR-8209): [`evaluate_lockstep`]
//! turns its record into the same [`ScenarioResult`], with the numbers in
//! [`ScenarioResult::lockstep`]. `involuntary_disconnects` and `no_wedged_client` are judged on its
//! seats, `desync_zero` and `lockstep_upload` on its record, and the bars that read snapshots are
//! skipped ([`LOCKSTEP_NO_SNAPSHOTS`]).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::impair::{DropCause, IP_UDP_OVERHEAD, LinkSpec};
use super::lockstep::{LockstepRecord, UPLOAD_BAR_RUN_MS, UPLOAD_P95_LIMIT_BPS, UPLOAD_PEAK_LIMIT_BPS};
use super::profiles::{BULK_SUFFIX, CONGESTED_DOWN_BPS, Profile, RTT_EXTREME_PEER, VARIANT_SUFFIXES};
use super::workload::{
    AddressMove, EndRecord, FRAME_MS, Histogram, HostileRecord, INPUT_GRACE_MS, Reconnect, RunRecord,
    STALENESS_STAGES, Scenario, StalenessParts, StalenessStages, TICK_HZ, WEDGE_MS, Wedge,
};

/// A rejoin must be connected this long after its link is usable, milliseconds.
pub const RECONNECT_GRACE_MS: u64 = 3_000;
/// An honest client is harmed when its snapshot staleness while the hostile peer is active exceeds
/// its own worst in the run's first 10 s (before the hostile peer starts) by more than this, ticks
/// (a third of a second at 30 Hz).
pub const HOSTILE_STALENESS_TICKS: u64 = 10;
/// `others_unaffected` leaves out a client's own outages and this long after each, milliseconds
/// (the next snapshot's way back), so a flappy client in a mixed run is judged on its link-up time
/// only (ADR-8454).
pub const OUTAGE_RECOVERY_MS: u64 = 1_000;
/// A hostile session must be dropped this long after its first hostile act, milliseconds.
pub const HOSTILE_DROP_MS: u64 = 5_000;
/// The silence policy the bars judge against: the engine's own,
/// `regia_net::TimeoutPolicy::default()` (ADR-7950: `interrupt_ms` of silence to the interruption
/// notice, `timeout_ms` to removal). The kit keeps no copy of these numbers. A run whose endpoints
/// follow another policy (`workload::run_with`) is still judged against this one, which is how a
/// broken policy shows up as a failed bar.
pub fn timeout_policy() -> regia_net::TimeoutPolicy {
    regia_net::TimeoutPolicy::default()
}
/// An end inside a long outage is the policy's removal when it comes no earlier than this before
/// the outage's start plus the removal timeout, milliseconds: a peer's silence starts at the last
/// datagram it got before the outage, not at the outage's start. An earlier end is a connection
/// the engine's policy would have kept, and no outage excuses it.
pub const REMOVAL_EARLY_MS: u64 = 1_000;
/// Downstream budget per client the sample game is designed for, bits per second.
pub const DOWN_BUDGET_BPS: u64 = 1_000_000;
/// Upstream budget per client, bits per second.
pub const UP_BUDGET_BPS: u64 = 256_000;
/// Format name of `result.json`.
pub const RESULT_FORMAT: &str = "regia-net-soak";
/// Format version of `result.json` (2 since ADR-8030: spec 120 bars, `measures`, `gating`,
/// `skipped`; [`SoakReport::from_json`] still reads version 1). The `lockstep` block of a lockstep
/// run (ADR-8209) is additive: a version 2 result without the key still loads.
pub const RESULT_VERSION: u32 = 2;
/// The bars that gate a soak when nothing overrides them: the harness's six (ADR-7899), then the
/// spec 120 bars their owning lanes have flipped (`xtask/src/net_soak.rs` `BARS`, which reaches
/// the soak as [`GATING_ENV`]): `lockstep_upload` since ADR-8209, `interrupted_once` and
/// `resume_same_peer` since ADR-10093.
pub const GATING_BARS: &[&str] = &[
    "involuntary_disconnects",
    "reconnect_within_grace",
    "no_wedged_client",
    "hostile_dropped",
    "others_unaffected",
    "bandwidth_within_budget",
    "interrupted_once",
    "resume_same_peer",
    "lockstep_upload",
];
/// The spec 120 § 15.2 bars (ADR-8030), in report order.
pub const SPEC120_BARS: [&str; 15] = [
    "missed_input",
    "corrections",
    "staleness_p99",
    "rate_vs_bottleneck",
    "queue_delay_p95",
    "rpc_p95_with_bulk",
    "random_loss_kept",
    "interrupted_once",
    "resume_same_peer",
    "no_hijack",
    "never_kicked_for_ping",
    "desync_zero",
    "lockstep_upload",
    "tick_p99",
    "join_under_flood",
];
/// The environment variable that lists the gating bars (comma-separated); `cargo xtask net-soak`
/// sets it from its table.
pub const GATING_ENV: &str = "REGIA_NET_SOAK_GATING";
/// `rate_vs_bottleneck`: the offered rate must be at or under the bottleneck this long after a
/// connect, milliseconds.
pub const RATE_SETTLE_MS: u64 = 5_000;
/// `queue_delay_p95`: limit, milliseconds.
pub const QUEUE_DELAY_P95_MS: u64 = 100;
/// `rpc_p95_with_bulk`: the bulk window's p95 may be at most this many times the control's.
pub const RPC_BULK_RATIO: f64 = 1.5;
/// `random_loss_kept`: the budget must stay at or above this fraction of a clean run's.
pub const RANDOM_LOSS_BUDGET: f64 = 0.9;
/// `join_under_flood`: a join counts when it connected within this long, milliseconds.
pub const JOIN_WITHIN_MS: u64 = 5_000;
/// `join_under_flood`: the fraction of joins that must connect in time.
pub const JOIN_FRACTION: f64 = 0.99;
/// One tick period at [`TICK_HZ`], microseconds: the ceiling of every `tick_p99` tier.
pub const TICK_P99_BUDGET_US: u32 = (1_000_000 / TICK_HZ) as u32;
/// `tick_p99` at up to 40 clients (the gate count): a quarter of the tick, microseconds, so the
/// game's own simulation keeps three quarters (docs/perf/budgets.ron `net_scale_tick_p99_40`,
/// ADR-8454).
pub const TICK_P99_BUDGET_40_US: u32 = TICK_P99_BUDGET_US / 4;
/// `tick_p99` at 41 to 64 clients: a third of the tick, microseconds (`net_scale_tick_p99_64`).
pub const TICK_P99_BUDGET_64_US: u32 = TICK_P99_BUDGET_US / 3;
/// `tick_p99` above 64 clients: half the tick, microseconds (`net_scale_tick_p99_128`).
pub const TICK_P99_BUDGET_128_US: u32 = TICK_P99_BUDGET_US / 2;

/// The `tick_p99` budget of a run with `clients` honest clients, microseconds: one CPU budget per
/// tier of spec 120 § 15.2 (40 gate, 64, 128; ADR-8454). The server tick here is the whole server
/// iteration of the soak's sample game (receive, simulate, snapshot for every client, flush), so
/// the share left over is the game's.
pub fn tick_budget_us(clients: usize) -> u32 {
    match clients {
        0..=40 => TICK_P99_BUDGET_40_US,
        41..=64 => TICK_P99_BUDGET_64_US,
        _ => TICK_P99_BUDGET_128_US,
    }
}
/// Reason of a bar a v1 result has no numbers for.
pub const NOT_MEASURED_V1: &str = "not measured in a v1 result";
/// `lockstep_upload`: the most seats the bar is stated for on the order-driven input mix
/// (spec 120 § 7.5; ADR-8209). A run with more seats reports its numbers and skips the bar.
pub const LOCKSTEP_UPLOAD_SEATS_ORDERS: usize = 40;
/// `lockstep_upload`: the most seats the bar is stated for on the action input mix, where half
/// the seats change their aim on any tick (ADR-8209).
pub const LOCKSTEP_UPLOAD_SEATS_ACTION: usize = 24;
/// `lockstep_upload`: the most seats the bar is stated for on the wander-fire bot mix. A held
/// heading changes less often than the order-driven mix, so the cap is the orders cap (ADR-8409).
pub const LOCKSTEP_UPLOAD_SEATS_BOTS: usize = LOCKSTEP_UPLOAD_SEATS_ORDERS;
/// `no_wedged_client` on a lockstep run: a seat has kept up when it stepped all but this many of
/// the frames the host built (the frames still in flight when the run ends).
pub const LOCKSTEP_KEEP_UP_FRAMES: u64 = 60;
/// Reason of a replication-only bar on a lockstep run.
pub const LOCKSTEP_NO_SNAPSHOTS: &str = "lockstep run: no snapshots";
/// Reason of a lockstep bar on a result without the lockstep numbers.
pub const NOT_A_LOCKSTEP_RESULT: &str = "no lockstep numbers in this result";

/// The branch whose fix a failure waits for, when it is a known blocker.
pub const BLOCKER_INPUT_MTU: &str = "v051-net-input-mtu";
/// The branch whose fix a failure waits for, when it is a known blocker.
pub const BLOCKER_INPUT_REJECTION: &str = "v051-net-input-rejection";

/// One bar's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bar {
    /// Bar id.
    pub id: String,
    /// Passed.
    pub passed: bool,
    /// What was measured, or every violation.
    pub detail: String,
    /// The known blockers every violation of this bar is attributed to; empty when any violation
    /// is not a known blocker (a new finding).
    pub known_blockers: Vec<String>,
    /// The bar decides the run's verdict (v1 bars all did).
    #[serde(default = "yes")]
    pub gating: bool,
    /// Why the bar was not judged (`needs: relay v2`, a v1 result without the numbers, a lockstep
    /// run beyond the seat count `lockstep_upload` is stated for); a skipped
    /// bar counts as passed.
    #[serde(default)]
    pub skipped: Option<String>,
}

fn yes() -> bool {
    true
}

/// The gating bar ids: [`GATING_ENV`] when set, else [`GATING_BARS`].
pub fn gating_bars() -> Vec<String> {
    match std::env::var(GATING_ENV) {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => GATING_BARS.iter().map(|s| s.to_string()).collect(),
    }
}

/// Summary numbers of one run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Clients that joined at least once.
    pub joined: usize,
    /// Involuntary ends by side and cause (`client:timeout`).
    pub ends_by_cause: BTreeMap<String, u64>,
    /// Every involuntary server-side end by cause, duplicates of client-side ends included.
    pub server_ends_by_cause: BTreeMap<String, u64>,
    /// Involuntary ends excused by an outage beyond the timeout policy.
    pub ends_excused: u64,
    /// Join or rejoin attempts the server refused, by code (`full`, ...).
    pub connect_refusals: BTreeMap<String, u64>,
    /// Join or rejoin attempts that timed out.
    pub connect_timeouts: u64,
    /// Rejoins.
    pub reconnects: u64,
    /// Rejoins that connected.
    pub reconnects_ok: u64,
    /// Longest rejoin time after the link was usable, milliseconds.
    pub reconnect_max_ms: u64,
    /// Inputs handed to the transport.
    pub inputs_sent: u64,
    /// Input send errors by kind.
    pub input_send_errors: BTreeMap<String, u64>,
    /// Inputs stamped behind the server.
    pub inputs_stamped_behind: u64,
    /// Server ticks with a client's input.
    pub server_ticks_with_input: u64,
    /// Server ticks without it (lost or late; outages excluded).
    pub server_ticks_missed: u64,
    /// Worst client's missed fraction.
    pub worst_missed_fraction: f64,
    /// Corrections.
    pub corrections: u64,
    /// Corrections per compared snapshot.
    pub correction_rate: f64,
    /// Correction size p50 / max, steps.
    pub correction_steps_p50: u64,
    /// Largest correction, steps.
    pub correction_steps_max: u64,
    /// Rollback depth p50, ticks.
    pub rollback_ticks_p50: u64,
    /// Rollback depth p99, ticks.
    pub rollback_ticks_p99: u64,
    /// Snapshot staleness p50, ticks.
    pub staleness_ticks_p50: u64,
    /// Snapshot staleness p99, ticks.
    pub staleness_ticks_p99: u64,
    /// Snapshot staleness max, ticks.
    pub staleness_ticks_max: u64,
    /// Highest per-client staleness p99, ticks.
    pub worst_client_staleness_p99: u64,
    /// Largest staleness of any client in each 10 s window of the run, ticks.
    pub staleness_max_by_10s: Vec<u64>,
    /// Each client's largest staleness per 10 s window, ticks (`[client][window]`).
    pub staleness_max_by_client_10s: Vec<Vec<u64>>,
    /// Reliable commands sent.
    pub commands_sent: u64,
    /// Reliable commands acknowledged.
    pub commands_acked: u64,
    /// Command round trip p99, milliseconds.
    pub command_rtt_p99_ms: u64,
    /// Mean downstream rate per client over the run, bits per second.
    pub down_bps_mean: u64,
    /// Highest client's mean downstream rate, bits per second.
    pub down_bps_max: u64,
    /// Highest client's mean offered downstream rate over the second half, bits per second.
    pub down_bps_second_half_max: u64,
    /// Mean upstream rate per client, bits per second.
    pub up_bps_mean: u64,
    /// Highest client's mean upstream rate, bits per second.
    pub up_bps_max: u64,
    /// Downstream budget, bits per second.
    pub down_budget_bps: u64,
    /// Upstream budget, bits per second.
    pub up_budget_bps: u64,
    /// Smallest entity budget the server gave a client after 5 s.
    pub min_entity_budget: Option<usize>,
    /// Server tick wall time p50, microseconds.
    pub server_tick_us_p50: u32,
    /// Server tick wall time p99, microseconds.
    pub server_tick_us_p99: u32,
    /// Server tick wall time max, microseconds.
    pub server_tick_us_max: u32,
    /// Peak heap growth of the run's thread (server, clients, network, reassembly buffers),
    /// bytes; 0 when the binary has no counting allocator.
    pub peak_heap_bytes: u64,
    /// Network drops by cause, every endpoint.
    pub drops: BTreeMap<String, u64>,
    /// Largest datagram any client sent, bytes.
    pub largest_client_datagram: usize,
    /// Largest datagram the server sent, bytes.
    pub largest_server_datagram: usize,
}

/// One run, evaluated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// `profile/N/mode`.
    pub label: String,
    /// The scenario.
    pub scenario: Scenario,
    /// Every bar passed.
    pub passed: bool,
    /// The bars.
    pub bars: Vec<Bar>,
    /// Summary numbers.
    pub metrics: Metrics,
    /// Involuntary ends.
    pub ends: Vec<EndRecord>,
    /// Rejoins.
    pub reconnects: Vec<Reconnect>,
    /// Wedges.
    pub wedges: Vec<Wedge>,
    /// The hostile peer.
    pub hostile: Option<HostileRecord>,
    /// The server endpoint's totals (datagrams, malformed, replays, rebinds, refusals).
    pub server_totals: regia_net::status::EndpointTotals,
    /// Wall time, milliseconds.
    pub wall_ms: u64,
    /// The command that reruns exactly this scenario.
    pub reproduce: String,
    /// The numbers the spec 120 bars read (v2; `None` in a v1 result).
    #[serde(default)]
    pub measures: Option<Measures>,
    /// Why the run did not happen (`needs: relay v2`).
    #[serde(default)]
    pub skipped: Option<String>,
    /// The numbers of a lockstep run (`lockstep-mix`; ADR-8209); `None` for a replication run and
    /// in a result written before the plan ran lockstep.
    #[serde(default)]
    pub lockstep: Option<LockstepMeasures>,
    /// The scale numbers (N215, ADR-8454; `None` in older results and skipped runs).
    #[serde(default)]
    pub scale: Option<ScaleMetrics>,
}

/// What a lockstep run measured, as `result.json` keeps it (ADR-8209): what `desync_zero`,
/// `lockstep_upload` and the harness's bars read on a `lockstep-mix` run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockstepMeasures {
    /// Seats in the match (the scenario's clients; on a listen host seat 0 is the host's own).
    pub seats: usize,
    /// The input mix: `orders` or `action`.
    pub mix: String,
    /// Frames the host built.
    pub frames_built: u64,
    /// The input delay at the end of the run, ticks.
    pub delay_ticks: u16,
    /// Delay raises.
    pub delay_raises: u64,
    /// Delay lowers.
    pub delay_lowers: u64,
    /// The host's upload over the whole seconds after the start, bits per second, IP and UDP
    /// headers included: the mean.
    pub upload_mean_bps: u64,
    /// The median whole second.
    pub upload_p50_bps: u64,
    /// The 95th-percentile whole second (nearest rank): `lockstep_upload` judges it against
    /// [`UPLOAD_P95_LIMIT_BPS`].
    pub upload_p95_bps: u64,
    /// The busiest whole second: `lockstep_upload` judges it against [`UPLOAD_PEAK_LIMIT_BPS`].
    pub upload_peak_bps: u64,
    /// Whole seconds the upload figures are taken over.
    pub upload_seconds: u64,
    /// Of those, seconds over [`UPLOAD_P95_LIMIT_BPS`].
    pub upload_seconds_over: u64,
    /// The host's upload, datagrams per second.
    pub upload_datagrams_per_s: u64,
    /// Desync verdicts.
    pub desync_verdicts: u64,
    /// State transfers taken, seats and host.
    pub resyncs: u64,
    /// The worst seat's ticks without its input, per thousand frames built.
    pub worst_missing_per_mille: u64,
    /// The longest stall after a single lost frames datagram on any seat, ticks.
    pub worst_single_loss_repair_ticks: u64,
    /// Frames the seats got from the reliable stream.
    pub stream_frames: u64,
    /// Times a seat reported a hole in its frames and was sent every unacknowledged frame.
    pub hole_repairs: u64,
    /// Checkpoints every voter agreed on.
    pub checkpoints_agreed: u64,
    /// Every simulation agrees at the last tick they all stepped.
    pub in_step: bool,
    /// Seats removed for divergence.
    pub removed: Vec<u16>,
    /// Seats the host stopped feeding because they fell a frame log behind.
    pub lagging: Vec<u16>,
    /// The votes could not be decided.
    pub unresolved: bool,
    /// Remote seats whose connection was accepted.
    pub seats_connected: usize,
    /// Remote seats that never connected.
    pub never_connected: Vec<u16>,
    /// Remote seats whose connection ended, with the end code.
    pub ended: Vec<(u16, String)>,
    /// Remote seats that stepped fewer than the frames built less [`LOCKSTEP_KEEP_UP_FRAMES`]:
    /// `(seat, frames stepped)`. A seat in `removed` or `lagging` is not listed again here.
    pub behind: Vec<(u16, u64)>,
    /// Payloads refused in the match: by the host after it started the frame clock, by the seats
    /// and by the transport.
    pub refused: u64,
    /// The mean input delay over the frames built, thousandths of a tick.
    #[serde(default)]
    pub delay_mean_milliticks: u64,
    /// The largest input delay a frame was built under, ticks.
    #[serde(default)]
    pub delay_max_ticks: u16,
    /// Seats the host flagged for holding their inputs back at any time in the run (late
    /// stamping, ADR-8209). Every seat of a soak run is honest, so a seat here is a false flag.
    #[serde(default)]
    pub late_stamp_flagged: Vec<u16>,
}

/// The numbers spec 120 § 15.3 and DO27's 40-seat record ask for (N215, ADR-8454): server CPU,
/// memory, bandwidth per seat, allocations per tick and why players left.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScaleMetrics {
    /// Honest clients.
    pub clients: usize,
    /// The datagram path (`virtual` or `udp`).
    pub wire: String,
    /// The `tick_p99` budget of this client count ([`tick_budget_us`]), microseconds.
    pub tick_budget_us: u32,
    /// Server tick wall time p50, microseconds.
    pub tick_us_p50: u32,
    /// Server tick wall time p99, microseconds.
    pub tick_us_p99: u32,
    /// Server tick wall time max, microseconds.
    pub tick_us_max: u32,
    /// Each phase of the ticking iterations (`workload::TICK_PHASES`): `(p50, p99)`, microseconds.
    pub tick_phase_us: BTreeMap<String, (u32, u32)>,
    /// The server's CPU time over the run's length: percent of one core at real time (every
    /// server iteration's wall time over the virtual run length; the run is single-threaded).
    pub server_cpu_pct_one_core: f64,
    /// Allocations per server tick, p50 (`None` without a counting allocator).
    pub allocs_per_tick_p50: Option<u32>,
    /// Allocations per server tick, p99.
    pub allocs_per_tick_p99: Option<u32>,
    /// Allocations per server tick, max.
    pub allocs_per_tick_max: Option<u32>,
    /// Allocations per server tick per client, mean.
    pub allocs_per_tick_per_client: Option<f64>,
    /// Server upstream over the run, every client together, Mbit/s (payload plus 28 bytes of
    /// IPv4 and UDP header per datagram).
    pub server_up_mbps: f64,
    /// Server upstream per honest client, kbit/s (same accounting).
    pub server_up_kbps_per_seat: f64,
    /// Server downstream per honest client, kbit/s (same accounting).
    pub server_down_kbps_per_seat: f64,
    /// Highest client's second-half offered downstream, bit/s, and the per-client budget.
    pub down_bps_second_half_max: u64,
    /// The per-client downstream budget, bit/s ([`DOWN_BUDGET_BPS`]).
    pub down_budget_bps: u64,
    /// Highest client's mean downstream over the run, bit/s.
    pub down_bps_max: u64,
    /// Highest client's mean upstream over the run, bit/s.
    pub up_bps_max: u64,
    /// The per-client upstream budget, bit/s ([`UP_BUDGET_BPS`]).
    pub up_budget_bps: u64,
    /// Clients whose mean downstream or upstream over the run was over its budget
    /// (`client 3 down 1200000 bit/s`); empty when every seat kept its budget.
    pub seats_over_budget: Vec<String>,
    /// Snapshot staleness p99 over every client, ticks.
    pub staleness_ticks_p99: u64,
    /// Every end, by `side:cause` (`server:timeout`, `client:left`), voluntary ones included.
    pub end_causes: BTreeMap<String, u64>,
    /// Involuntary ends not excused by an outage longer than the timeout policy.
    pub involuntary_disconnects: u64,
    /// The process's resident set at the run's end and its peak, KiB (whole process; Linux).
    pub rss_kib: Option<(u64, u64)>,
    /// The real UDP wire's counters.
    pub wire_stats: Option<super::impair::WireStats>,
}

/// The [`ScaleMetrics`] of a run.
pub fn scale_metrics(r: &RunRecord, metrics: &Metrics, involuntary: u64) -> ScaleMetrics {
    let s = &r.scenario;
    let clients = s.clients;
    let secs = (s.duration_ms as f64 / 1000.0).max(0.001);
    let wire_bits = |bytes: u64, datagrams: u64| ((bytes + datagrams * IP_UDP_OVERHEAD as u64) * 8) as f64;
    let n = &r.server_net;
    let up_bits = wire_bits(n.sent_bytes, n.sent);
    let down_bits = wire_bits(n.received_bytes, n.received);
    let per_seat = |bits: f64| bits / secs / 1000.0 / clients.max(1) as f64;
    let allocs = &r.server.tick_allocs;
    let has_allocs = !allocs.is_empty();
    let mut seats_over_budget = Vec::new();
    for (i, c) in r.client_net.iter().enumerate() {
        let down = (c.received_bytes as f64 * 8.0 / secs) as u64;
        let up = (c.sent_bytes as f64 * 8.0 / secs) as u64;
        if down > DOWN_BUDGET_BPS {
            seats_over_budget.push(format!("client {i} down {down} bit/s"));
        }
        if up > UP_BUDGET_BPS {
            seats_over_budget.push(format!("client {i} up {up} bit/s"));
        }
    }
    let mut tick_phase_us = BTreeMap::new();
    for (i, name) in super::workload::TICK_PHASES.iter().enumerate() {
        let v: Vec<u32> = r.server.tick_phases_us.iter().map(|p| p[i]).collect();
        if !v.is_empty() {
            tick_phase_us.insert(
                (*name).to_string(),
                (quantile_u32(&v, 0.5), quantile_u32(&v, 0.99)),
            );
        }
    }
    let mut end_causes = BTreeMap::new();
    for e in &r.ends {
        *end_causes.entry(format!("{}:{}", e.side, e.cause)).or_insert(0) += 1;
    }
    ScaleMetrics {
        clients,
        wire: s.wire.as_str().to_string(),
        tick_budget_us: tick_budget_us(clients),
        tick_us_p50: quantile_u32(&r.server.tick_us, 0.5),
        tick_us_p99: quantile_u32(&r.server.tick_us, 0.99),
        tick_us_max: r.server.tick_us.iter().copied().max().unwrap_or(0),
        tick_phase_us,
        server_cpu_pct_one_core: r.server.busy_us as f64 / (secs * 1_000_000.0) * 100.0,
        allocs_per_tick_p50: has_allocs.then(|| quantile_u32(allocs, 0.5)),
        allocs_per_tick_p99: has_allocs.then(|| quantile_u32(allocs, 0.99)),
        allocs_per_tick_max: allocs.iter().copied().max(),
        allocs_per_tick_per_client: has_allocs.then(|| {
            allocs.iter().map(|&a| f64::from(a)).sum::<f64>() / allocs.len() as f64 / clients.max(1) as f64
        }),
        server_up_mbps: up_bits / secs / 1_000_000.0,
        server_up_kbps_per_seat: per_seat(up_bits),
        server_down_kbps_per_seat: per_seat(down_bits),
        down_bps_second_half_max: metrics.down_bps_second_half_max,
        down_budget_bps: DOWN_BUDGET_BPS,
        down_bps_max: metrics.down_bps_max,
        up_bps_max: metrics.up_bps_max,
        up_budget_bps: UP_BUDGET_BPS,
        seats_over_budget,
        staleness_ticks_p99: metrics.staleness_ticks_p99,
        end_causes,
        involuntary_disconnects: involuntary,
        rss_kib: r.rss_kib,
        wire_stats: r.wire.clone(),
    }
}

/// One bottleneck's offered rate, second by second (`rate_vs_bottleneck`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateCheck {
    /// `client 3 down`, `server up`.
    pub link: String,
    /// The bottleneck, bits per second.
    pub cap_bps: u64,
    /// Whole seconds judged (from [`RATE_SETTLE_MS`] after each connect, link up).
    pub seconds_checked: u64,
    /// Of those, seconds whose offered wire rate (payload + [`IP_UDP_OVERHEAD`] per datagram) was over
    /// the cap.
    pub seconds_over: u64,
    /// First second over the cap.
    pub first_over_s: Option<u64>,
    /// Highest judged second, bits per second.
    pub worst_bps: u64,
}

/// Staleness of the clients that share one limit (`staleness_p99`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalenessGroup {
    /// The limit, ticks.
    pub limit_ticks: u64,
    /// Clients in the group.
    pub clients: Vec<usize>,
    /// Samples with the link up.
    pub samples: u64,
    /// Their p99, ticks.
    pub p99_ticks: u64,
}

/// Where the staleness of one sub-profile's clients comes from, link up (ADR-10096): the bar's
/// number, the same with the path's one-way latency taken off every sample, and the mean of each
/// part (`workload::StalenessParts`). Reported beside `staleness_p99`; no bar reads it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StalenessPath {
    /// The clients' sub-profile.
    pub profile: String,
    /// The `staleness_p99` limit of these clients, ticks.
    pub limit_ticks: u64,
    /// Clients in the group.
    pub clients: Vec<usize>,
    /// Samples with the link up.
    pub samples: u64,
    /// Their p99 as the bar counts it (the client's tick estimate minus its newest snapshot's
    /// tick), ticks.
    pub p99_ticks: u64,
    /// The p99 with the whole ticks of the path's one-way latency taken off each sample, ticks.
    pub beyond_path_p99_ticks: u64,
    /// The highest such p99 of any one client, ticks.
    pub worst_client_beyond_path_p99: u64,
    /// The fewest whole ticks of one-way latency any sample had taken off.
    pub path_ticks_min: u64,
    /// The most.
    pub path_ticks_max: u64,
    /// Mean of the bar's samples, ticks; the five means below add up to it.
    pub mean_ticks: f64,
    /// Mean one-way latency of the path (server uplink plus client downlink), ticks.
    pub mean_path_ticks: f64,
    /// Mean of the newest snapshot's way beyond the path: jitter, queueing, anything the sender
    /// held it for, the wait for the client's frame; ticks.
    pub mean_transit_ticks: f64,
    /// Mean time the newest snapshot had been the newest (the send interval, lost and late
    /// snapshots), ticks.
    pub mean_hold_ticks: f64,
    /// Mean cost of counting in whole ticks (the server is on tick `t + 1` when it sends the
    /// snapshot of tick `t`), ticks.
    pub mean_rounding_ticks: f64,
    /// Mean of the client's tick estimate minus the server's real tick, ticks.
    pub mean_estimate_ticks: f64,
}

/// The mean time a set of staleness samples spent in each stage, ticks (ADR-10145; the stages of
/// `workload::STALENESS_STAGES`). The seven add up to the samples' mean staleness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StageTicks {
    /// The server's send queue and pacer.
    pub sender_hold: f64,
    /// Waiting in the bottleneck queues of the route.
    pub queue: f64,
    /// The datagram's own transmission time at the bottlenecks.
    pub serialize: f64,
    /// Latency, jitter and reordering holds.
    pub path: f64,
    /// From the client's socket to the frame that handed the snapshot to the game.
    pub receiver_hold: f64,
    /// From that frame to the sample: the snapshot interval and every snapshot that did not
    /// arrive (lost, expired in the sender, withheld).
    pub wait: f64,
    /// The bar's count minus the snapshot's true age: the tick label, whole-tick counting and the
    /// error of the client's tick estimate (negative when the estimate runs behind).
    pub estimate: f64,
}

impl StageTicks {
    fn of(v: [f64; 7]) -> Self {
        let [sender_hold, queue, serialize, path, receiver_hold, wait, estimate] = v;
        Self {
            sender_hold,
            queue,
            serialize,
            path,
            receiver_hold,
            wait,
            estimate,
        }
    }

    /// The stages by name, in the order of `workload::STALENESS_STAGES`.
    pub fn named(&self) -> [(&'static str, f64); 7] {
        let v = [
            self.sender_hold,
            self.queue,
            self.serialize,
            self.path,
            self.receiver_hold,
            self.wait,
            self.estimate,
        ];
        std::array::from_fn(|i| (STALENESS_STAGES[i], v[i]))
    }

    /// The sum of the stages: the samples' mean staleness, ticks.
    pub fn total(&self) -> f64 {
        self.named().iter().map(|s| s.1).sum()
    }

    /// The stage that holds the most ticks.
    pub fn dominant(&self) -> (&'static str, f64) {
        self.named().into_iter().fold(
            ("", f64::NEG_INFINITY),
            |best, s| if s.1 > best.1 { s } else { best },
        )
    }
}

/// Where the staleness of the clients that share one limit went (ADR-10145): the per-stage
/// breakdown behind [`StalenessGroup`]. A report for diagnosis: no bar reads it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StalenessStageGroup {
    /// The limit, ticks.
    pub limit_ticks: u64,
    /// Samples with the link up that were traced to a datagram.
    pub samples: u64,
    /// Samples with the link up that could not be traced (the real UDP wire, a capture replay).
    pub untraced: u64,
    /// The group's p99 (as `staleness_p99` judges it), ticks.
    pub p99_ticks: u64,
    /// Samples with the link up over the limit, traced or not.
    pub over_limit_samples: u64,
    /// The mean stage times of every traced sample.
    pub mean: StageTicks,
    /// Traced samples at or over the p99.
    pub tail_samples: u64,
    /// Their mean stage times: where the ticks of the judged tail went.
    pub tail: StageTicks,
    /// The stage that holds most of the tail.
    pub dominant: String,
    /// Samples taken `workload::STALENESS_SETTLE_MS` or more after their connect.
    pub settled_samples: u64,
    /// Their p99, ticks: the run without its join transients.
    pub settled_p99_ticks: u64,
}

/// Command round trips before and during the bulk transfer (`rpc_p95_with_bulk`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcWithBulk {
    /// When the bulk transfer started, milliseconds.
    pub bulk_start_ms: u64,
    /// When the last client had all of it (the run end if one never did), milliseconds.
    pub bulk_end_ms: u64,
    /// Commands sent from 5 s to the bulk start.
    pub control_n: u64,
    /// Their round trip p95 (an unanswered command counts until the run end), milliseconds.
    pub control_p95_ms: u64,
    /// Commands sent while the bulk transfer was in flight.
    pub bulk_n: u64,
    /// Their round trip p95, milliseconds.
    pub bulk_p95_ms: u64,
}

/// One outage of one client (`interrupted_once`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutageCheck {
    /// Client index.
    pub client: usize,
    /// Outage start, milliseconds.
    pub start_ms: u64,
    /// Outage end, milliseconds.
    pub end_ms: u64,
    /// `Interrupted` events from the start to the next outage (or the run end).
    pub interrupted: u64,
    /// `Restored` events in the same span.
    pub restored: u64,
    /// The client was connected after the outage and did not leave on purpose inside it, so a
    /// `Restored` is owed.
    pub restore_owed: bool,
}

/// One involuntary rejoin (`resume_same_peer`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeCheck {
    /// Client index.
    pub client: usize,
    /// When the connection was lost, milliseconds.
    pub at_ms: u64,
    /// The peer id before.
    pub before_peer: u64,
    /// The peer id after (`None`: never back).
    pub after_peer: Option<u64>,
}

/// The numbers the spec 120 bars read that v1 did not record (ADR-8030).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Measures {
    /// Each client's sub-profile.
    pub client_profiles: Vec<String>,
    /// Staleness with the link up, grouped by limit.
    pub staleness: Vec<StalenessGroup>,
    /// The same samples by sub-profile, with the path's latency told apart from what is added
    /// (ADR-10096). Empty in a result written before it.
    #[serde(default)]
    pub staleness_paths: Vec<StalenessPath>,
    /// Every finite bottleneck of the run.
    pub rates: Vec<RateCheck>,
    /// Queueing delay p95 of every game datagram at the clients' bottlenecks (and the server's
    /// uplink when it has one), milliseconds.
    pub queue_delay_p95_ms: u64,
    /// Datagrams behind that p95.
    pub queue_delay_samples: u64,
    /// Command round trips around the bulk transfer.
    pub rpc_with_bulk: Option<RpcWithBulk>,
    /// Entities in the world: a clean run's snapshot budget.
    pub world_entities: usize,
    /// Outages of at least the engine's `interrupt_ms` ([`timeout_policy`]) while connected.
    pub outages: Vec<OutageCheck>,
    /// `Interrupted` events outside every outage.
    pub stray_interruptions: u64,
    /// Involuntary rejoins.
    pub resumes: Vec<ResumeCheck>,
    /// Changes of the server's address for a peer.
    pub address_moves: u64,
    /// Those to an address the client never held.
    pub foreign_moves: Vec<AddressMove>,
    /// Join attempts that had 5 s to finish.
    pub join_attempts: u64,
    /// Of those, attempts connected within [`JOIN_WITHIN_MS`].
    pub joins_in_time: u64,
    /// Clients that joined at least once, by index.
    pub joined_clients: Vec<usize>,
    /// Forged `ConnectRequest`s sent.
    pub forged_sent: u64,
    /// Client ticks judged for input (from each client's first input or 1 s after it joined, link
    /// up).
    pub input_ticks_judged: u64,
    /// Of those, ticks without the client's input at the server.
    pub input_ticks_missed: u64,
    /// The worst client's missed fraction.
    pub worst_client_missed: f64,
    /// Where the staleness went, stage by stage, grouped by limit as `staleness` is (ADR-10145).
    #[serde(default)]
    pub staleness_stages: Vec<StalenessStageGroup>,
}

/// The command that reruns one scenario.
pub fn reproduce_command(s: &Scenario) -> String {
    format!(
        "cargo xtask net-soak --profiles {} --clients {} --modes {} --seed {} --seconds {}",
        s.profile.name,
        s.clients,
        s.mode.as_str(),
        s.seed,
        s.duration_ms / 1000
    )
}

fn excused(r: &RunRecord, e: &EndRecord) -> bool {
    let timeout_ms = timeout_policy().timeout_ms;
    r.links.get(e.client).is_some_and(|l| {
        l.outages.iter().any(|&(s, end)| {
            // Only an outage the engine's policy ends a connection for excuses an end (one longer
            // than its removal timeout), and only an end the policy calls for: from the removal
            // time on, not before it.
            end - s > timeout_ms
                && e.at_ms + REMOVAL_EARLY_MS >= s + timeout_ms
                && e.at_ms <= end + RECONNECT_GRACE_MS
        })
    })
}

fn connected_through(r: &RunRecord, client: usize, peer: Option<u64>, from: u64, to: u64) -> bool {
    r.clients.get(client).is_some_and(|c| {
        c.connected
            .iter()
            .any(|&(p, s, e)| peer.is_none_or(|q| q == p) && s <= from && e >= to)
    })
}

fn heavy_window(s: &Scenario, client: usize) -> Option<(u64, u64)> {
    (client % 8 == 2).then_some((s.duration_ms / 5, s.duration_ms * 3 / 10))
}

/// The judged parts of a server input gap `[from, to)` of `client` as `peer` (ADR-8887): inside
/// the client's own connected intervals as that peer, and outside every outage of its link that
/// the gap overlaps, each widened by [`INPUT_GRACE_MS`] after its end (the grace a join gets: the
/// input sent into the outage is lost, the next one needs the link back). An outage the gap does
/// not overlap excuses nothing, so a gap with none is judged whole. Before ADR-8887 a gap was
/// judged only when the client was connected as the peer through all of it, and any outage it
/// touched excused all of it.
pub fn judged_input_gap(r: &RunRecord, client: usize, peer: u64, from: u64, to: u64) -> Vec<(u64, u64)> {
    let Some(c) = r.clients.get(client) else {
        return Vec::new();
    };
    let mut excused: Vec<(u64, u64)> = r
        .links
        .get(client)
        .map(|l| {
            l.outages
                .iter()
                .filter(|&&(s, e)| s < to && e > from)
                .map(|&(s, e)| (s, e.saturating_add(INPUT_GRACE_MS)))
                .collect()
        })
        .unwrap_or_default();
    excused.sort_unstable();
    let mut out = Vec::new();
    for &(p, cs, ce) in &c.connected {
        let (a, b) = (from.max(cs), to.min(ce));
        if p != peer || a >= b {
            continue;
        }
        let mut cursor = a;
        for &(s, e) in &excused {
            if e <= cursor || s >= b {
                continue;
            }
            if s > cursor {
                out.push((cursor, s));
            }
            cursor = cursor.max(e);
            if cursor >= b {
                break;
            }
        }
        if cursor < b {
            out.push((cursor, b));
        }
    }
    out
}

/// Wedges of a run: never-joined clients, the judged parts of the server's input gaps
/// ([`judged_input_gap`]) at least [`WEDGE_MS`] long, and the client-side gaps.
pub fn wedges(r: &RunRecord) -> Vec<Wedge> {
    let mut out = Vec::new();
    for c in &r.clients {
        if !c.joined {
            out.push(Wedge {
                client: c.client,
                kind: "never_joined".into(),
                start_ms: 0,
                length_ms: r.scenario.duration_ms,
                send_errors: BTreeMap::new(),
            });
        }
    }
    for &(client, peer, from, to) in &r.server.input_gaps {
        for (s, e) in judged_input_gap(r, client, peer, from, to) {
            if e - s < WEDGE_MS {
                continue;
            }
            out.push(Wedge {
                client,
                kind: "server_no_input".into(),
                start_ms: s,
                length_ms: e - s,
                send_errors: r
                    .clients
                    .get(client)
                    .map(|c| c.input_send_errors.clone())
                    .unwrap_or_default(),
            });
        }
    }
    for w in &r.client_gaps {
        if connected_through(r, w.client, None, w.start_ms, w.start_ms + w.length_ms) {
            out.push(w.clone());
        }
    }
    out.sort_by_key(|w| (w.client, w.start_ms));
    out
}

fn wedge_blocker(s: &Scenario, w: &Wedge) -> Option<&'static str> {
    let too_large = w.send_errors.get("too_large").copied().unwrap_or(0) > 0;
    let in_heavy =
        heavy_window(s, w.client).is_some_and(|(a, b)| w.start_ms < b && w.start_ms + w.length_ms > a);
    (too_large && (in_heavy || w.kind == "input_send_failing")).then_some(BLOCKER_INPUT_MTU)
}

fn bps(bytes_per_s: f64) -> u64 {
    (bytes_per_s * 8.0) as u64
}

fn mean(v: &[u64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<u64>() as f64 / v.len() as f64
    }
}

fn quantile_u32(v: &[u32], q: f64) -> u32 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let i = ((s.len() as f64 * q).ceil() as usize).clamp(1, s.len()) - 1;
    s[i]
}

fn bar(id: &str, violations: Vec<(String, Option<&'static str>)>, ok_detail: String) -> Bar {
    let passed = violations.is_empty();
    let all_known = !passed && violations.iter().all(|(_, b)| b.is_some());
    let mut known: Vec<String> = if all_known {
        violations
            .iter()
            .filter_map(|(_, b)| b.map(str::to_string))
            .collect()
    } else {
        Vec::new()
    };
    known.sort();
    known.dedup();
    let detail = if passed {
        ok_detail
    } else {
        violations
            .iter()
            .map(|(v, b)| match b {
                Some(b) => format!("{v} [known blocker {b}]"),
                None => v.clone(),
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    Bar {
        id: id.into(),
        passed,
        detail,
        known_blockers: known,
        gating: true,
        skipped: None,
    }
}

/// Evaluates a run. `peak_heap_bytes` comes from the caller's allocator counters.
pub fn evaluate(r: &RunRecord, peak_heap_bytes: u64) -> ScenarioResult {
    let s = &r.scenario;
    let d = s.duration_ms;
    let wedges = wedges(r);
    // One lost connection is one end: the client's own. A server-side end counts only when the
    // client still believed it was connected as that peer at the end of the run (a ghost link the
    // client never noticed); otherwise the client side already recorded it, or the client had left
    // on purpose and the server merely timed out its silence.
    let involuntary: Vec<&EndRecord> = r
        .ends
        .iter()
        .filter(|e| {
            !e.voluntary && (e.side == "client" || connected_through(r, e.client, Some(e.peer), e.at_ms, d))
        })
        .collect();
    let mut server_side = BTreeMap::new();
    for e in r.ends.iter().filter(|e| !e.voluntary && e.side == "server") {
        *server_side.entry(e.cause.clone()).or_insert(0u64) += 1;
    }
    let mut metrics = Metrics {
        joined: r.clients.iter().filter(|c| c.joined).count(),
        peak_heap_bytes,
        down_budget_bps: if s.profile.name == "congested" {
            CONGESTED_DOWN_BPS
        } else {
            DOWN_BUDGET_BPS
        },
        up_budget_bps: UP_BUDGET_BPS,
        min_entity_budget: r.server.min_entity_budget,
        server_ends_by_cause: server_side,
        ..Metrics::default()
    };
    for e in &involuntary {
        if excused(r, e) {
            metrics.ends_excused += 1;
        } else {
            *metrics
                .ends_by_cause
                .entry(format!("{}:{}", e.side, e.cause))
                .or_insert(0) += 1;
        }
    }
    // Rejoins.
    let mut reconnect_violations = Vec::new();
    for rc in &r.reconnects {
        metrics.reconnects += 1;
        let deadline = rc.link_up_ms + RECONNECT_GRACE_MS;
        match rc.connected_ms {
            Some(t) => {
                metrics.reconnects_ok += 1;
                metrics.reconnect_max_ms = metrics.reconnect_max_ms.max(t.saturating_sub(rc.link_up_ms));
                if t > deadline {
                    reconnect_violations.push((
                        format!(
                            "client {} {} rejoin took {} ms after its link was up (start {} ms)",
                            rc.client,
                            rc.kind,
                            t - rc.link_up_ms,
                            rc.start_ms
                        ),
                        None,
                    ));
                }
            }
            None if deadline <= d => reconnect_violations.push((
                format!(
                    "client {} {} rejoin from {} ms never connected (link up at {} ms)",
                    rc.client, rc.kind, rc.start_ms, rc.link_up_ms
                ),
                None,
            )),
            None => {}
        }
    }
    // Inputs, corrections, staleness, commands.
    let mut corr = Histogram::new(64);
    let mut rollback = Histogram::new(64);
    let mut stale = Histogram::new(256);
    let mut rtt = Histogram::new(512);
    let mut checked = 0;
    for c in &r.clients {
        metrics.inputs_sent += c.inputs_sent;
        for (k, v) in &c.input_send_errors {
            *metrics.input_send_errors.entry(k.clone()).or_insert(0) += v;
        }
        metrics.inputs_stamped_behind += c.inputs_stamped_behind;
        metrics.connect_timeouts += c.connect_timeouts;
        for (k, v) in &c.connect_refusals {
            *metrics.connect_refusals.entry(k.clone()).or_insert(0) += v;
        }
        metrics.corrections += c.corrections;
        checked += c.corrections_checked;
        metrics.commands_sent += c.commands_sent;
        metrics.commands_acked += c.commands_acked;
        corr.merge(&c.correction_steps);
        rollback.merge(&c.rollback_ticks);
        stale.merge(&c.staleness_ticks);
        metrics.worst_client_staleness_p99 = metrics
            .worst_client_staleness_p99
            .max(c.staleness_ticks.quantile(0.99));
        let mut mine: Vec<u64> = Vec::new();
        for (sec, &v) in c.staleness_max_per_s.iter().enumerate() {
            if mine.len() <= sec / 10 {
                mine.resize(sec / 10 + 1, 0);
            }
            mine[sec / 10] = mine[sec / 10].max(u64::from(v));
            let w = sec / 10;
            if metrics.staleness_max_by_10s.len() <= w {
                metrics.staleness_max_by_10s.resize(w + 1, 0);
            }
            metrics.staleness_max_by_10s[w] = metrics.staleness_max_by_10s[w].max(u64::from(v));
        }
        metrics.staleness_max_by_client_10s.push(mine);
        rtt.merge(&c.command_rtt_frames);
        let with = r.server.ticks_with_input.get(&c.client).copied().unwrap_or(0);
        let missed = r.server.ticks_missed.get(&c.client).copied().unwrap_or(0);
        metrics.server_ticks_with_input += with;
        metrics.server_ticks_missed += missed;
        if with + missed > 0 {
            metrics.worst_missed_fraction = metrics
                .worst_missed_fraction
                .max(missed as f64 / (with + missed) as f64);
        }
    }
    metrics.correction_rate = if checked == 0 {
        0.0
    } else {
        metrics.corrections as f64 / checked as f64
    };
    metrics.correction_steps_p50 = corr.quantile(0.5);
    metrics.correction_steps_max = corr.max;
    metrics.rollback_ticks_p50 = rollback.quantile(0.5);
    metrics.rollback_ticks_p99 = rollback.quantile(0.99);
    metrics.staleness_ticks_p50 = stale.quantile(0.5);
    metrics.staleness_ticks_p99 = stale.quantile(0.99);
    metrics.staleness_ticks_max = stale.max;
    metrics.command_rtt_p99_ms = rtt.quantile(0.99) * FRAME_MS;
    // Bandwidth.
    let secs = (d / 1000).max(1) as f64;
    let downs: Vec<u64> = r
        .client_net
        .iter()
        .map(|n| bps(n.received_bytes as f64 / secs))
        .collect();
    let ups: Vec<u64> = r
        .client_net
        .iter()
        .map(|n| bps(n.sent_bytes as f64 / secs))
        .collect();
    metrics.down_bps_mean = mean(&downs) as u64;
    metrics.down_bps_max = downs.iter().copied().max().unwrap_or(0);
    metrics.up_bps_mean = mean(&ups) as u64;
    metrics.up_bps_max = ups.iter().copied().max().unwrap_or(0);
    let half = (d / 2000) as usize;
    let end_s = (d / 1000) as usize;
    let second_half: Vec<(usize, u64)> = r
        .client_net
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let window: Vec<u64> = (half..end_s)
                .map(|t| n.down_offered_per_s.get(t).copied().unwrap_or(0))
                .collect();
            (i, bps(mean(&window)))
        })
        .collect();
    metrics.down_bps_second_half_max = second_half.iter().map(|&(_, b)| b).max().unwrap_or(0);
    metrics.server_tick_us_p50 = quantile_u32(&r.server.tick_us, 0.5);
    metrics.server_tick_us_p99 = quantile_u32(&r.server.tick_us, 0.99);
    metrics.server_tick_us_max = r.server.tick_us.iter().copied().max().unwrap_or(0);
    for n in r.client_net.iter().chain(std::iter::once(&r.server_net)) {
        for cause in DropCause::ALL {
            let v = n.drops_of(cause);
            if v > 0 {
                *metrics.drops.entry(cause.as_str().into()).or_insert(0) += v;
            }
        }
    }
    metrics.largest_client_datagram = r.client_net.iter().map(|n| n.largest_sent).max().unwrap_or(0);
    metrics.largest_server_datagram = r.server_net.largest_sent;

    let mut bars = Vec::new();
    let end_violations: Vec<(String, Option<&'static str>)> = involuntary
        .iter()
        .filter(|e| !excused(r, e))
        .map(|e| {
            (
                format!(
                    "client {} ended on the {} side at {} ms: {}",
                    e.client, e.side, e.at_ms, e.cause
                ),
                None,
            )
        })
        .collect();
    bars.push(bar(
        "involuntary_disconnects",
        end_violations,
        format!(
            "0 involuntary ends ({} excused by outages beyond the timeout)",
            metrics.ends_excused
        ),
    ));
    bars.push(bar(
        "reconnect_within_grace",
        reconnect_violations,
        format!(
            "{} rejoins, longest {} ms after the link was up (grace {RECONNECT_GRACE_MS} ms)",
            metrics.reconnects, metrics.reconnect_max_ms
        ),
    ));
    let wedge_violations = wedges
        .iter()
        .map(|w| {
            (
                format!(
                    "client {} {} for {} ms from {} ms (input send errors {:?})",
                    w.client, w.kind, w.length_ms, w.start_ms, w.send_errors
                ),
                wedge_blocker(s, w),
            )
        })
        .collect();
    bars.push(bar(
        "no_wedged_client",
        wedge_violations,
        format!("{} of {} clients joined, no wedge", metrics.joined, s.clients),
    ));
    if let Some(h) = &r.hostile {
        let mut v = Vec::new();
        for sess in &h.sessions {
            let Some(first) = sess.first_act_ms else {
                v.push((format!("hostile session {} never got to act", sess.kind), None));
                continue;
            };
            let blocker = (sess.kind == "over_cap_input").then_some(BLOCKER_INPUT_REJECTION);
            match sess.dropped_ms {
                Some(t) if t - first <= HOSTILE_DROP_MS => {}
                Some(t) => v.push((
                    format!(
                        "hostile session {} dropped {} ms after its first act ({}), over {HOSTILE_DROP_MS} ms",
                        sess.kind,
                        t - first,
                        sess.end_cause.as_deref().unwrap_or("?")
                    ),
                    blocker,
                )),
                None => v.push((
                    format!(
                        "hostile session {} was never dropped ({} hostile sends from {} ms to {} ms)",
                        sess.kind, sess.acts, first, sess.end_ms
                    ),
                    blocker,
                )),
            }
        }
        bars.push(bar(
            "hostile_dropped",
            v,
            format!(
                "{} hostile sessions, each dropped within {HOSTILE_DROP_MS} ms",
                h.sessions.len()
            ),
        ));
        let (from, to) = (
            h.sessions.first().map_or(0, |x| x.start_ms),
            h.sessions
                .last()
                .map_or(0, |x| x.end_ms + timeout_policy().timeout_ms),
        );
        let mut harmed: Vec<(String, Option<&'static str>)> = involuntary
            .iter()
            .filter(|e| e.at_ms >= from && e.at_ms <= to && !excused(r, e))
            .map(|e| {
                (
                    format!("client {} ended ({}) at {} ms", e.client, e.cause, e.at_ms),
                    None,
                )
            })
            .collect();
        harmed.extend(
            wedges
                .iter()
                .filter(|w| w.start_ms < to && w.start_ms + w.length_ms > from)
                .map(|w| {
                    (
                        format!("client {} {} at {} ms", w.client, w.kind, w.start_ms),
                        wedge_blocker(s, w),
                    )
                }),
        );
        for c in &r.clients {
            let s0 = (from / 1000) as usize;
            let s1 = ((to / 1000) as usize).min(c.staleness_max_per_s.len());
            let before = c.staleness_max_per_s.iter().take(s0).copied().max().unwrap_or(0);
            // Seconds inside the client's own outages (and the second after one, while the next
            // snapshot is on its way) are its link's doing, not the hostile peer's: `mixed+hostile`
            // has flappy clients (ADR-8454).
            let outages = r.links.get(c.client).map_or(&[][..], |l| &l.outages[..]);
            let in_own_outage = |sec: usize| {
                let (a, b) = (sec as u64 * 1000, sec as u64 * 1000 + 1000);
                outages
                    .iter()
                    .any(|&(start, end)| a < end + OUTAGE_RECOVERY_MS && b > start)
            };
            let during = (s0..s1.max(s0))
                .filter(|&sec| !in_own_outage(sec))
                .filter_map(|sec| c.staleness_max_per_s.get(sec).copied())
                .max()
                .unwrap_or(0);
            if u64::from(during) > u64::from(before) + HOSTILE_STALENESS_TICKS {
                harmed.push((
                    format!(
                        "client {} snapshot staleness reached {during} ticks while the hostile peer was active (before: {before})",
                        c.client
                    ),
                    None,
                ));
            }
        }
        bars.push(bar(
            "others_unaffected",
            harmed,
            format!(
                "no honest client ended or wedged between {from} and {to} ms ({} replays, {} garbage, {} oversized datagrams)",
                h.replays_sent, h.garbage_sent, h.oversized_sent
            ),
        ));
    }
    if s.profile.name == "congested" {
        let cap = s.profile.down.bandwidth_bps;
        let v = second_half
            .iter()
            .filter(|&&(_, b)| b > cap)
            .map(|&(i, b)| {
                (
                    format!(
                        "client {i} was offered {b} bit/s downstream over the second half, cap {cap} bit/s"
                    ),
                    None,
                )
            })
            .collect();
        bars.push(bar(
            "bandwidth_within_budget",
            v,
            format!(
                "highest second-half offered downstream {} bit/s, cap {cap} bit/s",
                metrics.down_bps_second_half_max
            ),
        ));
    }
    let mut out = ScenarioResult {
        label: s.label(),
        scenario: s.clone(),
        passed: true,
        bars,
        metrics,
        ends: involuntary.into_iter().cloned().collect(),
        reconnects: r.reconnects.clone(),
        wedges,
        hostile: r.hostile.clone(),
        server_totals: r.server.totals,
        wall_ms: r.wall_ms,
        reproduce: reproduce_command(s),
        measures: Some(measure(r)),
        skipped: None,
        lockstep: None,
        scale: None,
    };
    let unexcused = out.ends.iter().filter(|e| !excused(r, e)).count() as u64;
    out.scale = Some(scale_metrics(r, &out.metrics, unexcused));
    let extra = spec120_bars(&out);
    out.bars.extend(extra);
    apply_gating(&mut out, &gating_bars());
    out
}

/// The numbers of a lockstep run for `result.json` (ADR-8209).
pub fn measure_lockstep(s: &Scenario, r: &LockstepRecord) -> LockstepMeasures {
    let frames = r.frames_built;
    LockstepMeasures {
        seats: s.clients,
        mix: s.profile.lockstep_mix().unwrap_or("orders").to_string(),
        frames_built: frames,
        delay_ticks: r.host.delay_ticks,
        delay_raises: r.host.delay_raises,
        delay_lowers: r.host.delay_lowers,
        upload_mean_bps: r.host_upload_bps,
        upload_p50_bps: r.host_upload_p50_bps,
        upload_p95_bps: r.host_upload_p95_bps,
        upload_peak_bps: r.host_upload_peak_bps,
        upload_seconds: r.host_upload_seconds,
        upload_seconds_over: r.host_upload_seconds_over,
        upload_datagrams_per_s: r.host_upload_datagrams_per_s,
        desync_verdicts: r.verdicts.len() as u64,
        resyncs: r.seats.iter().map(|x| u64::from(x.resyncs)).sum::<u64>() + u64::from(r.host_resyncs),
        worst_missing_per_mille: r
            .host
            .seats
            .iter()
            .map(|row| row.inputs_missing.saturating_mul(1000) / frames.max(1))
            .max()
            .unwrap_or(0),
        worst_single_loss_repair_ticks: r
            .seats
            .iter()
            .map(|x| x.status.single_loss_repair_ticks_max)
            .max()
            .unwrap_or(0),
        stream_frames: r.seats.iter().map(|x| x.stream_frames).sum(),
        hole_repairs: r.host.seats.iter().map(|row| row.hole_repairs).sum(),
        checkpoints_agreed: r.host.checkpoints_agreed,
        in_step: r.in_step,
        removed: r.removed.clone(),
        lagging: r.lagging.clone(),
        unresolved: r.unresolved,
        seats_connected: r.seats.iter().filter(|x| x.connected_ms.is_some()).count(),
        never_connected: r
            .seats
            .iter()
            .filter(|x| x.connected_ms.is_none())
            .map(|x| x.seat)
            .collect(),
        ended: r
            .seats
            .iter()
            .filter_map(|x| x.ended.as_ref().map(|cause| (x.seat, cause.clone())))
            .collect(),
        // A seat removed for divergence or dropped as lagging stops stepping: that is
        // `desync_zero`'s finding, or listed as lagging, not a second one here.
        behind: r
            .seats
            .iter()
            .filter(|x| {
                x.connected_ms.is_some()
                    && x.frames_stepped + LOCKSTEP_KEEP_UP_FRAMES < frames
                    && !r.removed.contains(&x.seat)
                    && !r.lagging.contains(&x.seat)
            })
            .map(|x| (x.seat, x.frames_stepped))
            .collect(),
        refused: r.host_refused + r.transport_refused + r.seats.iter().map(|x| x.refused).sum::<u64>(),
        delay_mean_milliticks: r.delay_mean_milliticks,
        delay_max_ticks: r.delay_max_ticks,
        late_stamp_flagged: r
            .host
            .seats
            .iter()
            .filter(|row| row.late_stamp_flags > 0 || row.late_stamp_suspect)
            .map(|row| row.seat)
            .collect(),
    }
}

/// Evaluates a lockstep run (`lockstep-mix` and its variants; ADR-8209): `s` is the planned
/// scenario (its clients are the seats), `r` what `lockstep::run_lockstep` measured. `wall_ms` and
/// `peak_heap_bytes` come from the caller's clock and allocator counters.
///
/// Of the harness's bars, `involuntary_disconnects` fails when a seat's connection ended and
/// `no_wedged_client` when a seat never connected, did not keep up with the frames or was dropped
/// as lagging; `reconnect_within_grace` does not apply (the lockstep workload never rejoins). The
/// profile's spec 120 bars come from [`spec120_bars`], like every run's.
pub fn evaluate_lockstep(
    s: &Scenario,
    r: &LockstepRecord,
    wall_ms: u64,
    peak_heap_bytes: u64,
) -> ScenarioResult {
    let m = measure_lockstep(s, r);
    // A listen host plays seat 0 itself: it has no connection to lose.
    let local = usize::from(s.mode == super::workload::ServerMode::Listen && s.clients > 0);
    let remote = r.seats.len();
    let mut metrics = Metrics {
        joined: m.seats_connected + local,
        peak_heap_bytes,
        server_ticks_missed: r.host.seats.iter().map(|row| row.inputs_missing).sum(),
        worst_missed_fraction: m.worst_missing_per_mille as f64 / 1000.0,
        ..Metrics::default()
    };
    metrics.server_ticks_with_input =
        (m.frames_built * r.host.seats.len() as u64).saturating_sub(metrics.server_ticks_missed);
    // The seats that ended are in `lockstep.ended`; the lockstep record keeps no end time, so
    // `ends` (which carries one) stays empty.
    for (_, cause) in &m.ended {
        *metrics
            .ends_by_cause
            .entry(format!("client:{cause}"))
            .or_insert(0) += 1;
    }
    let mut bars = vec![
        judged(
            "involuntary_disconnects",
            m.ended
                .iter()
                .map(|(seat, cause)| format!("seat {seat}'s connection ended: {cause}"))
                .collect(),
            format!("0 of {remote} seat connections ended"),
        ),
        judged(
            "reconnect_within_grace",
            Vec::new(),
            "not applicable to a lockstep run: its workload never rejoins".into(),
        ),
    ];
    let mut wedged: Vec<String> = m
        .never_connected
        .iter()
        .map(|seat| format!("seat {seat} never connected"))
        .collect();
    wedged.extend(m.behind.iter().map(|(seat, stepped)| {
        format!(
            "seat {seat} stepped {stepped} of the {} frames built (more than {LOCKSTEP_KEEP_UP_FRAMES} behind)",
            m.frames_built
        )
    }));
    wedged.extend(
        m.lagging
            .iter()
            .map(|seat| format!("seat {seat} fell a frame log behind and was dropped from the frame stream")),
    );
    bars.push(judged(
        "no_wedged_client",
        wedged,
        format!(
            "{} of {remote} remote seats connected, each within {LOCKSTEP_KEEP_UP_FRAMES} frames of the {} built",
            m.seats_connected, m.frames_built
        ),
    ));
    let mut out = ScenarioResult {
        label: s.label(),
        scenario: s.clone(),
        passed: true,
        bars,
        metrics,
        ends: Vec::new(),
        reconnects: Vec::new(),
        wedges: Vec::new(),
        hostile: None,
        server_totals: regia_net::status::EndpointTotals::default(),
        wall_ms,
        reproduce: reproduce_command(s),
        measures: None,
        skipped: None,
        lockstep: Some(m),
        scale: None,
    };
    let extra = spec120_bars(&out);
    out.bars.extend(extra);
    apply_gating(&mut out, &gating_bars());
    out
}

/// Sets every bar's `gating` from `gating` and the run's verdict from the gating bars.
pub fn apply_gating(r: &mut ScenarioResult, gating: &[String]) {
    for b in &mut r.bars {
        b.gating = gating.iter().any(|g| g == &b.id);
    }
    r.passed = r.bars.iter().all(|b| b.passed || !b.gating);
}

/// The result of a run that was not made: `needs` names the missing engine part. Its spec 120 bars
/// are listed as skipped.
pub fn skipped_result(s: &Scenario, needs: &str) -> ScenarioResult {
    let reason = format!("needs: {needs}");
    let mut out = ScenarioResult {
        label: s.label(),
        scenario: s.clone(),
        passed: true,
        bars: Vec::new(),
        metrics: Metrics::default(),
        ends: Vec::new(),
        reconnects: Vec::new(),
        wedges: Vec::new(),
        hostile: None,
        server_totals: regia_net::status::EndpointTotals::default(),
        wall_ms: 0,
        reproduce: reproduce_command(s),
        measures: None,
        skipped: Some(reason.clone()),
        lockstep: None,
        scale: None,
    };
    out.bars = bars_for(&s.profile)
        .into_iter()
        .map(|id| skipped_bar(id, &reason))
        .collect();
    apply_gating(&mut out, &gating_bars());
    out
}

fn skipped_bar(id: &str, reason: &str) -> Bar {
    Bar {
        id: id.into(),
        passed: true,
        detail: format!("skipped: {reason}"),
        known_blockers: Vec::new(),
        gating: false,
        skipped: Some(reason.into()),
    }
}

/// The profile name without its `+bulk`, `+hostile`, `+action` or `+bots` suffixes.
pub fn base_name(p: &Profile) -> &str {
    let mut name = p.name.as_str();
    while let Some(base) = VARIANT_SUFFIXES.iter().find_map(|s| name.strip_suffix(s)) {
        name = base;
    }
    name
}

/// The spec 120 bars a profile is judged on, in report order.
pub fn bars_for(p: &Profile) -> Vec<&'static str> {
    let base = base_name(p);
    let mut out = Vec::new();
    if missed_limit(base).is_some() {
        out.push("missed_input");
    }
    if corrections_limit(base).is_some() {
        out.push("corrections");
    }
    out.push("staleness_p99");
    if matches!(base, "congested" | "bufferbloat" | "listen-uplink") {
        out.push("rate_vs_bottleneck");
    }
    if base == "bufferbloat" {
        out.push("queue_delay_p95");
    }
    if p.bulk.is_some() {
        out.push("rpc_p95_with_bulk");
    }
    if base == "lossy-fat-pipe" {
        out.push("random_loss_kept");
    }
    if matches!(base, "flappy" | "long-outage" | "good-wifi") {
        out.push("interrupted_once");
    }
    if matches!(base, "long-outage" | "wifi-to-cell") {
        out.push("resume_same_peer");
    }
    if p.hostile || matches!(base, "nat-rebind" | "cgnat") {
        out.push("no_hijack");
    }
    if base == "rtt-extreme" {
        out.push("never_kicked_for_ping");
    }
    if base == "lockstep-mix" {
        out.push("desync_zero");
        out.push("lockstep_upload");
    }
    if base == "mixed" {
        out.push("tick_p99");
    }
    if base == "forged-handshake" {
        out.push("join_under_flood");
    }
    out
}

fn missed_limit(base: &str) -> Option<f64> {
    match base {
        "clean" | "good-wifi" => Some(0.005),
        "bad-wifi" | "mobile-4g" => Some(0.02),
        "asym-bufferbloat" => Some(0.03),
        _ => None,
    }
}

fn corrections_limit(base: &str) -> Option<f64> {
    match base {
        "good-wifi" => Some(0.01),
        "bad-wifi" => Some(0.03),
        _ => None,
    }
}

fn staleness_limit(sub_profile: &str) -> u64 {
    if sub_profile.strip_suffix(BULK_SUFFIX).unwrap_or(sub_profile) == "congested" {
        8
    } else {
        4
    }
}

/// The link-up staleness of each sub-profile's clients, split into the path and what is added
/// (ADR-10096).
fn staleness_paths(r: &RunRecord) -> Vec<StalenessPath> {
    struct Group {
        clients: Vec<usize>,
        raw: Histogram,
        beyond: Histogram,
        worst: u64,
        parts: StalenessParts,
    }
    let mut groups: BTreeMap<&str, Group> = BTreeMap::new();
    for c in &r.clients {
        let g = groups.entry(c.profile.as_str()).or_insert_with(|| Group {
            clients: Vec::new(),
            raw: Histogram::new(256),
            beyond: Histogram::new(256),
            worst: 0,
            parts: StalenessParts::default(),
        });
        g.clients.push(c.client);
        g.raw.merge(&c.staleness_ticks_link_up);
        g.beyond.merge(&c.staleness_beyond_path_link_up);
        g.worst = g.worst.max(c.staleness_beyond_path_link_up.quantile(0.99));
        g.parts.merge(&c.staleness_parts);
    }
    groups
        .into_iter()
        .filter(|(_, g)| g.parts.samples > 0)
        .map(|(profile, g)| {
            let p = &g.parts;
            StalenessPath {
                profile: profile.to_string(),
                limit_ticks: staleness_limit(profile),
                clients: g.clients,
                samples: p.samples,
                p99_ticks: g.raw.quantile(0.99),
                beyond_path_p99_ticks: g.beyond.quantile(0.99),
                worst_client_beyond_path_p99: g.worst,
                path_ticks_min: p.path_ticks_min,
                path_ticks_max: p.path_ticks_max,
                mean_ticks: p.mean_ticks(p.total),
                mean_path_ticks: p.mean_ticks(p.path),
                mean_transit_ticks: p.mean_ticks(p.transit),
                mean_hold_ticks: p.mean_ticks(p.hold),
                mean_rounding_ticks: p.mean_ticks(p.rounding),
                mean_estimate_ticks: p.mean_ticks(p.estimate),
            }
        })
        .collect()
}

fn wire_bps(bytes: &[u64], datagrams: &[u64], second: usize) -> u64 {
    let b = bytes.get(second).copied().unwrap_or(0);
    let d = datagrams.get(second).copied().unwrap_or(0);
    (b + d * IP_UDP_OVERHEAD as u64) * 8
}

fn judge_rate(link: String, cap_bps: u64, seconds: impl Iterator<Item = (u64, u64)>) -> RateCheck {
    let mut c = RateCheck {
        link,
        cap_bps,
        ..RateCheck::default()
    };
    for (sec, bps) in seconds {
        c.seconds_checked += 1;
        c.worst_bps = c.worst_bps.max(bps);
        if bps > cap_bps {
            c.seconds_over += 1;
            c.first_over_s.get_or_insert(sec);
        }
    }
    c
}

/// Whole seconds `[a, a + 1 s)` inside `[from_ms, to_ms)` with the link up.
fn judged_seconds(link: &LinkSpec, from_ms: u64, to_ms: u64) -> impl Iterator<Item = u64> + '_ {
    (from_ms.div_ceil(1000)..to_ms / 1000)
        .filter(move |&sec| !link.outage_overlaps(sec * 1000, sec * 1000 + 1000))
}

fn p95_ms(mut v: Vec<u64>) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 * 0.95).ceil() as usize).clamp(1, v.len()) - 1;
    v[i]
}

/// The v2 numbers of a run (ADR-8030).
pub fn measure(r: &RunRecord) -> Measures {
    let s = &r.scenario;
    let d = s.duration_ms;
    let mut m = Measures {
        client_profiles: r.clients.iter().map(|c| c.profile.clone()).collect(),
        world_entities: r.server.world_entities,
        forged_sent: r.forged_sent,
        address_moves: r.server.address_moves.len() as u64,
        foreign_moves: r
            .server
            .address_moves
            .iter()
            .filter(|m| m.foreign)
            .cloned()
            .collect(),
        joined_clients: r.clients.iter().filter(|c| c.joined).map(|c| c.client).collect(),
        ..Measures::default()
    };
    // Input ticks, a client whose input never arrives included.
    for (c, &judged) in &r.server.input_ticks_judged {
        let arrived = r.server.input_ticks_arrived.get(c).copied().unwrap_or(0);
        m.input_ticks_judged += judged;
        m.input_ticks_missed += judged - arrived;
        if judged > 0 {
            m.worst_client_missed = m
                .worst_client_missed
                .max((judged - arrived) as f64 / judged as f64);
        }
    }
    // Staleness with the link up, grouped by limit.
    let mut groups: BTreeMap<u64, (Vec<usize>, Histogram)> = BTreeMap::new();
    let mut stage_groups: BTreeMap<u64, (StalenessStages, Histogram)> = BTreeMap::new();
    for c in &r.clients {
        let g = groups
            .entry(staleness_limit(&c.profile))
            .or_insert_with(|| (Vec::new(), Histogram::new(256)));
        g.0.push(c.client);
        g.1.merge(&c.staleness_ticks_link_up);
        let sg = stage_groups
            .entry(staleness_limit(&c.profile))
            .or_insert_with(|| (StalenessStages::default(), Histogram::new(256)));
        sg.0.merge(&c.staleness_stages);
        if sg.1.buckets.len() < c.staleness_ticks_settled.buckets.len() {
            sg.1.buckets.resize(c.staleness_ticks_settled.buckets.len(), 0);
        }
        sg.1.merge(&c.staleness_ticks_settled);
    }
    m.staleness_stages = stage_groups
        .into_iter()
        .filter_map(|(limit_ticks, (stages, settled))| {
            let h = &groups.get(&limit_ticks)?.1;
            let p99_ticks = h.quantile(0.99);
            let (samples, mean) = stages.mean_ticks(0, TICK_HZ);
            let (tail_samples, tail) = stages.mean_ticks(p99_ticks, TICK_HZ);
            let tail = StageTicks::of(tail);
            Some(StalenessStageGroup {
                limit_ticks,
                samples,
                untraced: stages.untraced,
                p99_ticks,
                over_limit_samples: h
                    .buckets
                    .iter()
                    .enumerate()
                    .filter(|(v, _)| *v as u64 > limit_ticks)
                    .map(|(_, n)| n)
                    .sum(),
                mean: StageTicks::of(mean),
                tail_samples,
                tail,
                dominant: if tail_samples == 0 {
                    String::new()
                } else {
                    tail.dominant().0.to_string()
                },
                settled_samples: settled.count(),
                settled_p99_ticks: settled.quantile(0.99),
            })
        })
        .collect();
    m.staleness = groups
        .into_iter()
        .map(|(limit_ticks, (clients, h))| StalenessGroup {
            limit_ticks,
            clients,
            samples: h.count(),
            p99_ticks: h.quantile(0.99),
        })
        .collect();
    m.staleness_paths = staleness_paths(r);
    // Offered rate at every finite bottleneck, from 5 s after each connect.
    for (i, (c, n)) in r.clients.iter().zip(&r.client_net).enumerate() {
        let Some(link) = r.links.get(i) else { continue };
        for (dir, spec, bytes, dgrams) in [
            (
                "down",
                &link.down,
                &n.down_offered_per_s,
                &n.down_offered_datagrams_per_s,
            ),
            ("up", &link.up, &n.up_offered_per_s, &n.up_offered_datagrams_per_s),
        ] {
            if spec.bandwidth_bps == 0 {
                continue;
            }
            let secs = c.connected.iter().flat_map(|&(_, from, to)| {
                judged_seconds(link, from + RATE_SETTLE_MS, to.min(d))
                    .map(move |sec| (sec, wire_bps(bytes, dgrams, sec as usize)))
            });
            m.rates
                .push(judge_rate(format!("client {i} {dir}"), spec.bandwidth_bps, secs));
        }
    }
    let server_link = s
        .profile
        .server_link(s.mode == super::workload::ServerMode::Listen);
    if server_link.up.bandwidth_bps > 0 {
        let first = r
            .clients
            .iter()
            .filter_map(|c| c.connected.first().map(|x| x.1))
            .max()
            .unwrap_or(d);
        let n = &r.server_net;
        let secs = judged_seconds(&server_link, first + RATE_SETTLE_MS, d).map(|sec| {
            (
                sec,
                wire_bps(&n.up_offered_per_s, &n.up_offered_datagrams_per_s, sec as usize),
            )
        });
        m.rates
            .push(judge_rate("server up".into(), server_link.up.bandwidth_bps, secs));
    }
    // Queueing delay of game datagrams.
    let mut q = Histogram::new(0);
    for n in r.client_net.iter().chain(std::iter::once(&r.server_net)) {
        for h in [&n.up_queue_delay_ms, &n.down_queue_delay_ms] {
            if q.buckets.len() < h.buckets.len() {
                q.buckets.resize(h.buckets.len(), 0);
            }
            q.merge(h);
        }
    }
    m.queue_delay_samples = q.count();
    m.queue_delay_p95_ms = q.quantile(0.95);
    // Command round trips around the bulk transfer.
    if let Some(start) = r.server.bulk_start_ms {
        let end = if r.clients.iter().all(|c| c.bulk_done_ms.is_some()) {
            r.clients.iter().filter_map(|c| c.bulk_done_ms).max().unwrap_or(d)
        } else {
            d
        };
        let (mut control, mut bulk) = (Vec::new(), Vec::new());
        for c in &r.clients {
            for &(sent, acked) in &c.command_log {
                let rtt = acked.unwrap_or(d).saturating_sub(sent);
                if sent >= 5_000 && sent < start {
                    control.push(rtt);
                } else if sent >= start && sent < end {
                    bulk.push(rtt);
                }
            }
        }
        m.rpc_with_bulk = Some(RpcWithBulk {
            bulk_start_ms: start,
            bulk_end_ms: end,
            control_n: control.len() as u64,
            control_p95_ms: p95_ms(control),
            bulk_n: bulk.len() as u64,
            bulk_p95_ms: p95_ms(bulk),
        });
    }
    // Interruptions per outage.
    let interrupt_ms = timeout_policy().interrupt_ms;
    for (i, c) in r.clients.iter().enumerate() {
        let Some(link) = r.links.get(i) else { continue };
        let mut covered = vec![false; c.interruptions.len()];
        for (k, &(start, end)) in link.outages.iter().enumerate() {
            if end - start < interrupt_ms || !c.connected.iter().any(|&(_, a, b)| a <= start && b > start) {
                continue;
            }
            let span_end = link.outages.get(k + 1).map_or(d, |n| n.0);
            let mut o = OutageCheck {
                client: i,
                start_ms: start,
                end_ms: end,
                // A client that left on purpose inside the outage ended the interruption itself:
                // its link is closed, so no `Restored` can follow (ADR-10093). Every other
                // connection that is up again after the outage owes one, whether it lasted
                // through it or timed out and reconnected.
                restore_owed: c.connected.iter().any(|&(_, _, b)| b > end)
                    && !c.leaves.iter().any(|&t| t >= start && t <= end),
                ..OutageCheck::default()
            };
            for (j, (at, kind)) in c.interruptions.iter().enumerate() {
                if *at >= start && *at < span_end {
                    covered[j] = true;
                    match kind.as_str() {
                        "interrupted" => o.interrupted += 1,
                        "restored" => o.restored += 1,
                        _ => {}
                    }
                }
            }
            m.outages.push(o);
        }
        m.stray_interruptions += c
            .interruptions
            .iter()
            .zip(&covered)
            .filter(|((_, kind), cov)| !**cov && kind == "interrupted")
            .count() as u64;
    }
    // Involuntary rejoins: the peer before and after.
    for rc in r.reconnects.iter().filter(|x| x.kind == "involuntary") {
        let Some(c) = r.clients.get(rc.client) else {
            continue;
        };
        let before = c
            .connected
            .iter()
            .filter(|x| x.2 <= rc.start_ms)
            .max_by_key(|x| x.2)
            .map_or(0, |x| x.0);
        let after = rc
            .connected_ms
            .and_then(|t| c.connected.iter().find(|x| x.1 == t).map(|x| x.0));
        m.resumes.push(ResumeCheck {
            client: rc.client,
            at_ms: rc.start_ms,
            before_peer: before,
            after_peer: after,
        });
    }
    // Joins with 5 s to finish.
    for c in &r.clients {
        for &(start, done) in &c.join_attempts {
            if start + JOIN_WITHIN_MS > d && done.is_none() {
                continue;
            }
            m.join_attempts += 1;
            if done.is_some_and(|t| t - start <= JOIN_WITHIN_MS) {
                m.joins_in_time += 1;
            }
        }
    }
    m
}

fn judged(id: &str, violations: Vec<String>, ok: String) -> Bar {
    bar(id, violations.into_iter().map(|v| (v, None)).collect(), ok)
}

/// Frames a lockstep run must have built before "no checkpoint was agreed" is a finding: the
/// first checkpoints need a round trip to be voted on.
const DESYNC_ZERO_MIN_FRAMES: u64 = 120;

/// What `desync_zero` holds against a lockstep run.
fn desync_violations(l: &LockstepMeasures) -> Vec<String> {
    let mut v = Vec::new();
    if l.desync_verdicts > 0 {
        v.push(format!("{} desync verdicts", l.desync_verdicts));
    }
    if l.resyncs > 0 {
        v.push(format!("{} state transfers (seats and host)", l.resyncs));
    }
    if !l.removed.is_empty() {
        v.push(format!("seats {:?} were removed for divergence", l.removed));
    }
    if l.unresolved {
        v.push("the votes could not be decided (desync_unresolved)".into());
    }
    if !l.in_step {
        v.push("the simulations do not agree at the last tick they all stepped".into());
    }
    // Zero desyncs mean nothing when no state hash was ever compared.
    if l.checkpoints_agreed == 0 && l.frames_built >= DESYNC_ZERO_MIN_FRAMES {
        v.push(format!(
            "no checkpoint was agreed over {} frames: nothing was compared",
            l.frames_built
        ));
    }
    v
}

/// `lockstep_upload` of a lockstep run of `duration_ms` (spec 120 § 7.5 as amended by ADR-8209).
fn lockstep_upload_bar(l: &LockstepMeasures, duration_ms: u64) -> Bar {
    let id = "lockstep_upload";
    let stated = if l.mix == "action" {
        LOCKSTEP_UPLOAD_SEATS_ACTION
    } else if l.mix == "bots" {
        LOCKSTEP_UPLOAD_SEATS_BOTS
    } else {
        LOCKSTEP_UPLOAD_SEATS_ORDERS
    };
    let run = if duration_ms == UPLOAD_BAR_RUN_MS {
        format!("{} s run", duration_ms / 1000)
    } else {
        format!(
            "{} s run (the bar is stated over {} s)",
            duration_ms / 1000,
            UPLOAD_BAR_RUN_MS / 1000
        )
    };
    let numbers = format!(
        "{} seats, {} mix, {run}: host upload mean {} bit/s, 95th-percentile second {} bit/s (limit \
         {UPLOAD_P95_LIMIT_BPS}), busiest second {} bit/s (limit {UPLOAD_PEAK_LIMIT_BPS}), {} of {} seconds over \
         {UPLOAD_P95_LIMIT_BPS}, {} datagrams/s",
        l.seats,
        l.mix,
        l.upload_mean_bps,
        l.upload_p95_bps,
        l.upload_peak_bps,
        l.upload_seconds_over,
        l.upload_seconds,
        l.upload_datagrams_per_s
    );
    if l.seats > stated {
        let reason = format!(
            "beyond the {stated} seats the bar is stated for on the {} mix",
            l.mix
        );
        let mut b = skipped_bar(id, &reason);
        b.detail = format!("skipped: {reason}; measured: {numbers}");
        return b;
    }
    let mut over = Vec::new();
    if l.upload_p95_bps > UPLOAD_P95_LIMIT_BPS {
        over.push("the 95th-percentile second");
    }
    if l.upload_peak_bps > UPLOAD_PEAK_LIMIT_BPS {
        over.push("the busiest second");
    }
    let violations = if l.upload_seconds == 0 {
        vec![format!("no whole second of host upload was measured: {numbers}")]
    } else if over.is_empty() {
        Vec::new()
    } else {
        vec![format!("{} over the limit: {numbers}", over.join(" and "))]
    };
    judged(id, violations, numbers)
}

/// The spec 120 § 15.2 bars of one run (ADR-8030), from its `result.json` entry alone. A v1 result
/// (no `measures`) is judged where its numbers allow, and the other bars are skipped.
pub fn spec120_bars(res: &ScenarioResult) -> Vec<Bar> {
    let base = base_name(&res.scenario.profile);
    let m = res.measures.as_ref();
    let met = &res.metrics;
    let p = &res.scenario.profile;
    let clients = res.scenario.clients;
    let mut out = Vec::new();
    for id in bars_for(p) {
        // A lockstep run has no snapshots, predictions or rejoins: only its own two bars read it.
        if res.lockstep.is_some() && !matches!(id, "desync_zero" | "lockstep_upload") {
            out.push(skipped_bar(id, LOCKSTEP_NO_SNAPSHOTS));
            continue;
        }
        let b = match id {
            "missed_input" => {
                let limit = missed_limit(base).unwrap_or(0.0);
                // v2 judges every client tick from 1 s after the join; v1 recorded only the ticks
                // after a client's first input arrived.
                let (missed, total, worst) = match m {
                    Some(m) => (m.input_ticks_missed, m.input_ticks_judged, m.worst_client_missed),
                    None => (
                        met.server_ticks_missed,
                        met.server_ticks_missed + met.server_ticks_with_input,
                        met.worst_missed_fraction,
                    ),
                };
                let frac = missed as f64 / total.max(1) as f64;
                let v = if total == 0 && clients > 0 {
                    vec!["no client tick was judged for input".to_string()]
                } else if frac > limit {
                    vec![format!(
                        "{:.2} % of {total} client ticks had no input at the server (limit {:.1} %; worst client {:.2} %)",
                        100.0 * frac,
                        100.0 * limit,
                        100.0 * worst
                    )]
                } else {
                    Vec::new()
                };
                judged(
                    id,
                    v,
                    format!(
                        "{:.2} % of {total} client ticks missed input (limit {:.1} %)",
                        100.0 * frac,
                        100.0 * limit
                    ),
                )
            }
            "corrections" => {
                let limit = corrections_limit(base).unwrap_or(0.0);
                let v = if met.correction_rate > limit {
                    vec![format!(
                        "{} corrections, {:.2} % of compared predictions (limit {:.1} %)",
                        met.corrections,
                        100.0 * met.correction_rate,
                        100.0 * limit
                    )]
                } else {
                    Vec::new()
                };
                judged(
                    id,
                    v,
                    format!(
                        "{:.2} % of compared predictions corrected (limit {:.1} %)",
                        100.0 * met.correction_rate,
                        100.0 * limit
                    ),
                )
            }
            "staleness_p99" => match m {
                Some(m) => {
                    let v = m
                        .staleness
                        .iter()
                        .filter(|g| g.p99_ticks > g.limit_ticks)
                        .map(|g| {
                            format!(
                                "staleness p99 {} ticks with the link up over {} samples of clients {:?} (limit {})",
                                g.p99_ticks, g.samples, g.clients, g.limit_ticks
                            )
                        })
                        .collect();
                    let ok = m
                        .staleness
                        .iter()
                        .map(|g| format!("p99 {} ticks (limit {})", g.p99_ticks, g.limit_ticks))
                        .collect::<Vec<_>>()
                        .join(", ");
                    judged(id, v, format!("staleness with the link up: {ok}"))
                }
                // v1 recorded staleness with outages included: usable only where there are none.
                None if p.flaps.is_none()
                    && p.long_outage_ms.is_none()
                    && p.mix.is_empty()
                    && base != "wifi-to-cell"
                    && base != "long-outage" =>
                {
                    let limit = staleness_limit(base);
                    let v = if met.staleness_ticks_p99 > limit {
                        vec![format!(
                            "staleness p99 {} ticks (limit {limit})",
                            met.staleness_ticks_p99
                        )]
                    } else {
                        Vec::new()
                    };
                    judged(
                        id,
                        v,
                        format!("staleness p99 {} ticks (limit {limit})", met.staleness_ticks_p99),
                    )
                }
                None => skipped_bar(id, NOT_MEASURED_V1),
            },
            "rate_vs_bottleneck" => match m {
                Some(m) => {
                    let v = m
                        .rates
                        .iter()
                        .filter(|c| c.seconds_over > 0)
                        .map(|c| {
                            format!(
                                "{}: {} of {} seconds over {} bit/s (first at {} s, worst {} bit/s)",
                                c.link,
                                c.seconds_over,
                                c.seconds_checked,
                                c.cap_bps,
                                c.first_over_s.unwrap_or(0),
                                c.worst_bps
                            )
                        })
                        .collect();
                    let worst = m
                        .rates
                        .iter()
                        .map(|c| c.worst_bps as f64 / c.cap_bps.max(1) as f64)
                        .fold(0.0, f64::max);
                    judged(
                        id,
                        v,
                        format!(
                            "{} bottlenecks held from 5 s after each connect (worst second at {:.0} % of its cap)",
                            m.rates.len(),
                            100.0 * worst
                        ),
                    )
                }
                None => skipped_bar(id, NOT_MEASURED_V1),
            },
            "queue_delay_p95" => match m {
                Some(m) => {
                    let v = if m.queue_delay_p95_ms > QUEUE_DELAY_P95_MS {
                        vec![format!(
                            "queueing delay p95 {} ms over {} datagrams (limit {QUEUE_DELAY_P95_MS} ms)",
                            m.queue_delay_p95_ms, m.queue_delay_samples
                        )]
                    } else {
                        Vec::new()
                    };
                    judged(
                        id,
                        v,
                        format!(
                            "queueing delay p95 {} ms over {} datagrams",
                            m.queue_delay_p95_ms, m.queue_delay_samples
                        ),
                    )
                }
                None => skipped_bar(id, NOT_MEASURED_V1),
            },
            "rpc_p95_with_bulk" => match m.and_then(|m| m.rpc_with_bulk.as_ref()) {
                Some(x) => {
                    let limit = x.control_p95_ms as f64 * RPC_BULK_RATIO;
                    let v = if x.control_n == 0 || x.bulk_n == 0 {
                        vec![format!(
                            "too few commands to compare ({} before, {} during the bulk transfer)",
                            x.control_n, x.bulk_n
                        )]
                    } else if x.bulk_p95_ms as f64 > limit {
                        vec![format!(
                            "command round trip p95 {} ms during the bulk transfer ({} commands, {}-{} ms) vs {} ms before it ({} commands): {:.1}x, limit {RPC_BULK_RATIO}x",
                            x.bulk_p95_ms,
                            x.bulk_n,
                            x.bulk_start_ms,
                            x.bulk_end_ms,
                            x.control_p95_ms,
                            x.control_n,
                            x.bulk_p95_ms as f64 / x.control_p95_ms.max(1) as f64
                        )]
                    } else {
                        Vec::new()
                    };
                    judged(
                        id,
                        v,
                        format!(
                            "command round trip p95 {} ms during the bulk transfer vs {} ms before",
                            x.bulk_p95_ms, x.control_p95_ms
                        ),
                    )
                }
                None if m.is_some() => {
                    judged(id, vec!["the bulk transfer never started".into()], String::new())
                }
                None => skipped_bar(id, NOT_MEASURED_V1),
            },
            "random_loss_kept" => {
                let full = m.map_or_else(
                    || clients + (140usize.saturating_sub(clients)).max(64),
                    |m| m.world_entities,
                );
                let need = (full as f64 * RANDOM_LOSS_BUDGET).ceil() as usize;
                let v = match met.min_entity_budget {
                    Some(b) if b >= need => Vec::new(),
                    Some(b) => vec![format!(
                        "the snapshot budget fell to {b} of {full} entities (needs >= {need}, 90 % of a clean run's)"
                    )],
                    None => vec!["no snapshot budget measured after 5 s".into()],
                };
                judged(
                    id,
                    v,
                    format!("the snapshot budget stayed >= {need} of {full} entities"),
                )
            }
            "interrupted_once" => match m {
                Some(m) => {
                    let mut v: Vec<String> = m
                        .outages
                        .iter()
                        .filter(|o| o.interrupted != 1 || (o.restore_owed && o.restored != 1))
                        .map(|o| {
                            format!(
                                "client {} outage {}-{} ms: {} Interrupted, {} Restored (want 1 and {})",
                                o.client,
                                o.start_ms,
                                o.end_ms,
                                o.interrupted,
                                o.restored,
                                u8::from(o.restore_owed)
                            )
                        })
                        .collect();
                    if m.stray_interruptions > 0 {
                        v.push(format!(
                            "{} Interrupted events outside any outage",
                            m.stray_interruptions
                        ));
                    }
                    judged(
                        id,
                        v,
                        format!(
                            "{} outages, each one Interrupted and one Restored; none elsewhere",
                            m.outages.len()
                        ),
                    )
                }
                None => skipped_bar(id, NOT_MEASURED_V1),
            },
            "resume_same_peer" => match m {
                Some(m) => {
                    let v = m
                        .resumes
                        .iter()
                        .filter(|x| x.after_peer != Some(x.before_peer))
                        .map(|x| match x.after_peer {
                            Some(a) => format!(
                                "client {} lost peer {} at {} ms and came back as peer {a} (its avatar is gone)",
                                x.client, x.before_peer, x.at_ms
                            ),
                            None => format!("client {} lost peer {} at {} ms and never came back", x.client, x.before_peer, x.at_ms),
                        })
                        .collect();
                    judged(
                        id,
                        v,
                        format!(
                            "{} involuntary rejoins, each with its old peer id",
                            m.resumes.len()
                        ),
                    )
                }
                None => skipped_bar(id, NOT_MEASURED_V1),
            },
            "no_hijack" => match m {
                Some(m) => {
                    let v = m
                        .foreign_moves
                        .iter()
                        .map(|x| {
                            format!(
                                "the server moved client {}'s peer {} to an address the client never held at {} ms",
                                x.client, x.peer, x.at_ms
                            )
                        })
                        .collect();
                    judged(
                        id,
                        v,
                        format!(
                            "{} address moves, all to addresses their clients held",
                            m.address_moves
                        ),
                    )
                }
                // v1: with no scheduled address change in the profile, every server-side move is
                // someone else's.
                None if p.rebind_at.is_none() && p.hostile => {
                    let n = res.server_totals.rebinds;
                    let v = if n > 0 {
                        vec![format!(
                            "{n} address moves with no address change scheduled (moves without a validated path)"
                        )]
                    } else {
                        Vec::new()
                    };
                    judged(id, v, "0 address moves".into())
                }
                None => skipped_bar(id, NOT_MEASURED_V1),
            },
            "never_kicked_for_ping" => {
                let extreme: Vec<usize> = (0..clients)
                    .filter(|&i| p.client_profile(i) == RTT_EXTREME_PEER)
                    .collect();
                let mut v: Vec<String> = res
                    .ends
                    .iter()
                    .filter(|e| extreme.contains(&e.client))
                    .map(|e| {
                        format!(
                            "rtt-extreme client {} ended ({}) at {} ms",
                            e.client, e.cause, e.at_ms
                        )
                    })
                    .collect();
                if let Some(m) = m {
                    v.extend(
                        extreme
                            .iter()
                            .filter(|i| !m.joined_clients.contains(i))
                            .map(|i| format!("rtt-extreme client {i} never joined")),
                    );
                }
                judged(
                    id,
                    v,
                    format!("{} rtt-extreme clients stayed the whole run", extreme.len()),
                )
            }
            "desync_zero" => match res.lockstep.as_ref() {
                Some(l) => judged(
                    id,
                    desync_violations(l),
                    format!(
                        "0 desync verdicts and 0 state transfers; {} checkpoints agreed over {} frames, {} seats in step",
                        l.checkpoints_agreed, l.frames_built, l.seats
                    ),
                ),
                None => skipped_bar(id, NOT_A_LOCKSTEP_RESULT),
            },
            "lockstep_upload" => match res.lockstep.as_ref() {
                Some(l) => lockstep_upload_bar(l, res.scenario.duration_ms),
                None => skipped_bar(id, NOT_A_LOCKSTEP_RESULT),
            },
            "tick_p99" => {
                let budget = tick_budget_us(clients);
                let v = if met.server_tick_us_p99 > budget {
                    vec![format!(
                        "server tick p99 {} us at {clients} clients (budget {budget} us of the {TICK_P99_BUDGET_US} us tick at {TICK_HZ} Hz)",
                        met.server_tick_us_p99
                    )]
                } else {
                    Vec::new()
                };
                judged(
                    id,
                    v,
                    format!(
                        "server tick p99 {} us (budget {budget} us at {clients} clients)",
                        met.server_tick_us_p99
                    ),
                )
            }
            "join_under_flood" => match m {
                Some(m) => {
                    let frac = m.joins_in_time as f64 / m.join_attempts.max(1) as f64;
                    let v = if m.join_attempts == 0 || frac < JOIN_FRACTION {
                        vec![format!(
                            "{} of {} join attempts connected within {} s ({:.1} %, needs {:.0} %) under {} forged ConnectRequests",
                            m.joins_in_time,
                            m.join_attempts,
                            JOIN_WITHIN_MS / 1000,
                            100.0 * frac,
                            100.0 * JOIN_FRACTION,
                            m.forged_sent
                        )]
                    } else {
                        Vec::new()
                    };
                    judged(
                        id,
                        v,
                        format!(
                            "{} of {} joins within 5 s under {} forged requests",
                            m.joins_in_time, m.join_attempts, m.forged_sent
                        ),
                    )
                }
                None => skipped_bar(id, NOT_MEASURED_V1),
            },
            _ => continue,
        };
        out.push(b);
    }
    for b in &mut out {
        b.gating = false;
    }
    out
}

/// The whole soak.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoakReport {
    /// [`RESULT_FORMAT`].
    pub format: String,
    /// [`RESULT_VERSION`].
    pub version: u32,
    /// Plan name (`gate`, `nightly` or `custom`).
    pub plan: String,
    /// Commit or label the caller passed.
    pub label: String,
    /// Every bar of every run passed.
    pub passed: bool,
    /// Failed bars whose every violation is a known blocker.
    pub known_blocker_failures: usize,
    /// Failed gating bars with at least one violation that is not a known blocker.
    pub new_failures: usize,
    /// Failed bars that do not gate yet (spec 120 bars before their owning lane flips them).
    #[serde(default)]
    pub non_gating_failures: usize,
    /// Every run.
    pub results: Vec<ScenarioResult>,
    /// Ticks per second of the server.
    pub tick_hz: u64,
}

impl SoakReport {
    /// Collects results.
    pub fn new(plan: &str, label: &str, results: Vec<ScenarioResult>) -> Self {
        let mut r = Self {
            format: RESULT_FORMAT.into(),
            version: RESULT_VERSION,
            plan: plan.into(),
            label: label.into(),
            passed: true,
            known_blocker_failures: 0,
            new_failures: 0,
            non_gating_failures: 0,
            results,
            tick_hz: TICK_HZ,
        };
        r.recount();
        r
    }

    /// Recomputes the verdict and the failure counts from the bars.
    pub fn recount(&mut self) {
        let failed: Vec<&Bar> = self
            .results
            .iter()
            .flat_map(|r| r.bars.iter())
            .filter(|b| !b.passed)
            .collect();
        let gating: Vec<&&Bar> = failed.iter().filter(|b| b.gating).collect();
        self.passed = gating.is_empty();
        self.known_blocker_failures = gating.iter().filter(|b| !b.known_blockers.is_empty()).count();
        self.new_failures = gating.iter().filter(|b| b.known_blockers.is_empty()).count();
        self.non_gating_failures = failed.len() - gating.len();
    }

    /// Reads a `result.json` of version 1 or 2. A v1 file gets the spec 120 bars it can be judged
    /// on (non-gating, so its verdict does not change; bars it has no numbers for are skipped) and
    /// keeps `version: 1` so a reader knows where it came from.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        if v.get("format").and_then(serde_json::Value::as_str) != Some(RESULT_FORMAT) {
            return Err(format!("not a {RESULT_FORMAT} result"));
        }
        let version = v.get("version").and_then(serde_json::Value::as_u64).unwrap_or(0);
        if version == 0 || version > u64::from(RESULT_VERSION) {
            return Err(format!(
                "{RESULT_FORMAT} version {version}: this build reads 1 to {RESULT_VERSION}"
            ));
        }
        let mut report: Self =
            serde_json::from_value(v).map_err(|e| format!("{RESULT_FORMAT} v{version}: {e}"))?;
        if version == 1 {
            for r in &mut report.results {
                if !r.bars.iter().any(|b| SPEC120_BARS.contains(&b.id.as_str())) {
                    let extra = spec120_bars(r);
                    r.bars.extend(extra);
                }
            }
            report.recount();
        }
        Ok(report)
    }

    /// The Markdown summary.
    pub fn summary_md(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "# Net soak: {} ({})\n", self.plan, self.label);
        let _ = writeln!(
            s,
            "Verdict: **{}**. Failed bars: {} known-blocker, {} new.\n",
            if self.passed { "PASS" } else { "FAIL" },
            self.known_blocker_failures,
            self.new_failures
        );
        let _ = writeln!(
            s,
            "| Run | Verdict | Joined | Invol. ends | Rejoins (max ms) | Missed input (worst) | Corrections | Staleness p99 (ticks) | Down kbit/s mean/max | Tick p99 µs | Peak heap MiB | Wall s |"
        );
        let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|---|");
        for r in &self.results {
            let m = &r.metrics;
            let ends: u64 = m.ends_by_cause.values().sum();
            if r.lockstep.is_some() {
                // A lockstep run has no rejoins, predictions or snapshots: its own numbers are in
                // the lockstep table below.
                let _ = writeln!(
                    s,
                    "| {} | {} | {}/{} | {} | - | {:.2} % ({:.1} %) | - | - | - | - | {:.1} | {:.1} |",
                    r.label,
                    if r.passed { "pass" } else { "FAIL" },
                    m.joined,
                    r.scenario.clients,
                    ends,
                    100.0 * m.server_ticks_missed as f64
                        / (m.server_ticks_missed + m.server_ticks_with_input).max(1) as f64,
                    100.0 * m.worst_missed_fraction,
                    m.peak_heap_bytes as f64 / (1024.0 * 1024.0),
                    r.wall_ms as f64 / 1000.0
                );
                continue;
            }
            let _ = writeln!(
                s,
                "| {} | {} | {}/{} | {} | {} ({}) | {:.2} % ({:.1} %) | {:.1} % | {} | {}/{} | {} | {:.1} | {:.1} |",
                r.label,
                if r.passed { "pass" } else { "FAIL" },
                m.joined,
                r.scenario.clients,
                ends,
                m.reconnects,
                m.reconnect_max_ms,
                100.0 * m.server_ticks_missed as f64
                    / (m.server_ticks_missed + m.server_ticks_with_input).max(1) as f64,
                100.0 * m.worst_missed_fraction,
                100.0 * m.correction_rate,
                m.staleness_ticks_p99,
                m.down_bps_mean / 1000,
                m.down_bps_max / 1000,
                m.server_tick_us_p99,
                m.peak_heap_bytes as f64 / (1024.0 * 1024.0),
                r.wall_ms as f64 / 1000.0
            );
        }
        self.lockstep_md(&mut s);
        self.staleness_stages_md(&mut s);
        let _ = writeln!(s, "\n## Failed bars\n");
        let mut any = false;
        for r in &self.results {
            for b in r.bars.iter().filter(|b| !b.passed && b.gating) {
                any = true;
                let tag = if b.known_blockers.is_empty() {
                    "NEW".to_string()
                } else {
                    format!("known blocker {}", b.known_blockers.join(", "))
                };
                let mut detail = b.detail.clone();
                if detail.len() > 1_500 {
                    let mut cut = 1_500;
                    while !detail.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    detail.truncate(cut);
                    detail.push_str(" ...");
                }
                let _ = writeln!(
                    s,
                    "- **{}** `{}` ({tag}; seed {}): {}\n  - reproduce: `{}`",
                    r.label, b.id, r.scenario.seed, detail, r.reproduce
                );
            }
        }
        if !any {
            let _ = writeln!(s, "None.");
        }
        let _ = writeln!(
            s,
            "\n## Spec 120 bars (not gating until their owning lane flips them; ADR-8030)\n\n{} failed.\n",
            self.non_gating_failures
        );
        let _ = writeln!(s, "| Run | Bar | Verdict | Detail |");
        let _ = writeln!(s, "|---|---|---|---|");
        for r in &self.results {
            if let Some(why) = &r.skipped {
                let _ = writeln!(s, "| {} | (run) | skipped | {why} |", r.label);
            }
            for b in r.bars.iter().filter(|b| !b.gating) {
                let verdict = if b.skipped.is_some() {
                    "skipped"
                } else if b.passed {
                    "pass"
                } else {
                    "FAIL"
                };
                let mut detail = b.detail.replace('|', "/");
                if detail.len() > 300 {
                    let mut cut = 300;
                    while !detail.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    detail.truncate(cut);
                    detail.push_str(" ...");
                }
                let _ = writeln!(s, "| {} | `{}` | {verdict} | {detail} |", r.label, b.id);
            }
        }
        self.staleness_paths_md(&mut s);
        let scaled: Vec<(&str, &ScaleMetrics)> = self
            .results
            .iter()
            .filter_map(|r| r.scale.as_ref().map(|m| (r.label.as_str(), m)))
            .filter(|(_, m)| m.clients >= 16)
            .collect();
        if !scaled.is_empty() {
            let _ = writeln!(
                s,
                "\n## Scale numbers (runs of 16+ clients; N215, ADR-8454)\n\n| Run | Tick p50/p99/max µs (budget) | Phase p99 µs (receive/events/harness/tick/flush) | Server CPU % of a core | Allocs/tick p50/p99 (per client) | Server up Mbit/s (kbit/s per seat) | Down per seat kbit/s | Client down/up max kbit/s (budget) | Staleness p99 | Involuntary / ends by cause | RSS end/peak MiB |\n|---|---|---|---|---|---|---|---|---|---|---|"
            );
            for (label, m) in scaled {
                let allocs = match (
                    m.allocs_per_tick_p50,
                    m.allocs_per_tick_p99,
                    m.allocs_per_tick_per_client,
                ) {
                    (Some(a), Some(b), Some(c)) => format!("{a}/{b} ({c:.1})"),
                    _ => "not counted".into(),
                };
                let phases: Vec<String> = super::workload::TICK_PHASES
                    .iter()
                    .map(|p| {
                        m.tick_phase_us
                            .get(*p)
                            .map_or("-".into(), |&(_, p99)| p99.to_string())
                    })
                    .collect();
                let causes: Vec<String> = m.end_causes.iter().map(|(k, v)| format!("{k} {v}")).collect();
                let rss = m.rss_kib.map_or("n/a".into(), |(a, b)| {
                    format!("{:.0}/{:.0}", a as f64 / 1024.0, b as f64 / 1024.0)
                });
                let _ = writeln!(
                    s,
                    "| {label} | {}/{}/{} ({}) | {} | {:.1} | {allocs} | {:.2} ({:.0}) | {:.0} | {}/{} ({}/{}){} | {} | {} / {} | {rss} |",
                    m.tick_us_p50,
                    m.tick_us_p99,
                    m.tick_us_max,
                    m.tick_budget_us,
                    phases.join("/"),
                    m.server_cpu_pct_one_core,
                    m.server_up_mbps,
                    m.server_up_kbps_per_seat,
                    m.server_down_kbps_per_seat,
                    m.down_bps_max / 1000,
                    m.up_bps_max / 1000,
                    m.down_budget_bps / 1000,
                    m.up_budget_bps / 1000,
                    if m.seats_over_budget.is_empty() {
                        String::new()
                    } else {
                        format!(", {} over", m.seats_over_budget.len())
                    },
                    m.staleness_ticks_p99,
                    m.involuntary_disconnects,
                    if causes.is_empty() {
                        "none".into()
                    } else {
                        causes.join(", ")
                    },
                );
            }
        }
        s
    }

    /// The staleness table of the summary (ADR-10096): every sub-profile whose link-up p99 is
    /// over its `staleness_p99` limit, with the path's one-way latency told apart from what the
    /// sender, the client and the tick estimate add. Nothing when no group is over.
    fn staleness_paths_md(&self, s: &mut String) {
        let over: Vec<(&str, &StalenessPath)> = self
            .results
            .iter()
            .filter_map(|r| r.measures.as_ref().map(|m| (r.label.as_str(), m)))
            .flat_map(|(label, m)| m.staleness_paths.iter().map(move |g| (label, g)))
            .filter(|(_, g)| g.p99_ticks > g.limit_ticks)
            .collect();
        if over.is_empty() {
            return;
        }
        let _ = writeln!(
            s,
            "\n## Staleness over the limit: the path and what is added (link up; ADR-10096)\n\n\
             `p99` is the bar's count (the client's tick estimate minus its newest snapshot's tick); \
             `beyond the path` takes the whole ticks of the one-way latency toward the client off every \
             sample. The means add up: path + transit + hold + rounding + estimate.\n\n\
             | Run | Clients' profile (clients) | Limit | p99 | Path ticks | p99 beyond the path (worst client) | Mean | Path | Transit | Hold | Rounding | Estimate |\n\
             |---|---|---|---|---|---|---|---|---|---|---|---|"
        );
        for (label, g) in over {
            let path = if g.path_ticks_min == g.path_ticks_max {
                g.path_ticks_min.to_string()
            } else {
                format!("{}-{}", g.path_ticks_min, g.path_ticks_max)
            };
            let _ = writeln!(
                s,
                "| {label} | {} ({}) | {} | {} | {path} | {} ({}) | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:+.2} |",
                g.profile,
                g.clients.len(),
                g.limit_ticks,
                g.p99_ticks,
                g.beyond_path_p99_ticks,
                g.worst_client_beyond_path_p99,
                g.mean_ticks,
                g.mean_path_ticks,
                g.mean_transit_ticks,
                g.mean_hold_ticks,
                g.mean_rounding_ticks,
                g.mean_estimate_ticks
            );
        }
    }

    /// The staleness-by-stage table of the summary (ADR-10145): one row per limit group whose
    /// `staleness_p99` is over its limit, with the mean time its judged tail (the samples at or
    /// over the p99) spent in each stage. Nothing when no group is over.
    fn staleness_stages_md(&self, s: &mut String) {
        let rows: Vec<(&str, &StalenessStageGroup)> = self
            .results
            .iter()
            .flat_map(|r| {
                r.measures
                    .iter()
                    .flat_map(|m| m.staleness_stages.iter())
                    .filter(|g| g.p99_ticks > g.limit_ticks)
                    .map(move |g| (r.label.as_str(), g))
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        let _ = writeln!(
            s,
            "\n## Staleness by stage (ADR-10145): groups over their limit\n\nMean ticks of the samples at or over the group's p99, by stage; the stages add up to the staleness (`estimate` is negative when the client's tick estimate runs behind). \"Settled\" is the p99 of the samples 5 s or more after their connect. A report for diagnosis: `staleness_p99` judges the p99 column.\n"
        );
        let _ = writeln!(
            s,
            "| Run | Limit | p99 | Over-limit samples (of traced) | Sender hold | Queue | Serialize | Path | Receiver hold | Wait | Estimate | Most ticks in | Settled p99 |"
        );
        let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for (label, g) in rows {
            let t = &g.tail;
            let _ = writeln!(
                s,
                "| {label} | {} | {} | {} ({}) | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:+.1} | {} | {} |",
                g.limit_ticks,
                g.p99_ticks,
                g.over_limit_samples,
                g.samples,
                t.sender_hold,
                t.queue,
                t.serialize,
                t.path,
                t.receiver_hold,
                t.wait,
                t.estimate,
                if g.tail_samples == 0 {
                    "untraced"
                } else {
                    g.dominant.as_str()
                },
                g.settled_p99_ticks
            );
        }
    }

    /// The lockstep runs' table of the summary (ADR-8209): one row per `lockstep-mix` run with the
    /// upload figures `lockstep_upload` is judged on and both lockstep bars' verdicts, whether they
    /// gate or not. Nothing when the plan had no lockstep run.
    fn lockstep_md(&self, s: &mut String) {
        if self.results.iter().all(|r| r.lockstep.is_none()) {
            return;
        }
        let _ = writeln!(s, "\n## Lockstep runs (ADR-8209)\n");
        let _ = writeln!(
            s,
            "| Run | Verdict | Seats (mix) | Frames | Delay ticks: at the end (raises/lowers), mean, largest | Upload bit/s: mean / median / p95 / busiest second | Seconds over {UPLOAD_P95_LIMIT_BPS} | `lockstep_upload` | Desyncs / resyncs | `desync_zero` | Missing input, worst seat | Worst single-loss stall, ticks | Stream frames / hole repairs | Seats flagged for late stamping | Wall s |"
        );
        let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for r in &self.results {
            let Some(l) = &r.lockstep else { continue };
            let verdict = |id: &str| match r.bars.iter().find(|b| b.id == id) {
                Some(b) => match &b.skipped {
                    Some(why) => format!("skipped: {why}"),
                    None if b.passed => "pass".to_string(),
                    None => "FAIL".to_string(),
                },
                None => "-".to_string(),
            };
            let _ = writeln!(
                s,
                "| {} | {} | {} ({}) | {} | {} ({}/{}), {}.{:02}, {} | {} / {} / {} / {} | {} of {} | {} | {} / {} | {} | {:.1} % | {} | {} / {} | {:?} | {:.1} |",
                r.label,
                if r.passed { "pass" } else { "FAIL" },
                l.seats,
                l.mix,
                l.frames_built,
                l.delay_ticks,
                l.delay_raises,
                l.delay_lowers,
                l.delay_mean_milliticks / 1000,
                l.delay_mean_milliticks % 1000 / 10,
                l.delay_max_ticks,
                l.upload_mean_bps,
                l.upload_p50_bps,
                l.upload_p95_bps,
                l.upload_peak_bps,
                l.upload_seconds_over,
                l.upload_seconds,
                verdict("lockstep_upload"),
                l.desync_verdicts,
                l.resyncs,
                verdict("desync_zero"),
                l.worst_missing_per_mille as f64 / 10.0,
                l.worst_single_loss_repair_ticks,
                l.stream_frames,
                l.hole_repairs,
                l.late_stamp_flagged,
                r.wall_ms as f64 / 1000.0
            );
        }
    }

    /// Writes `result.json` and `summary.md` into `dir` (created), each atomically.
    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        write_atomic(&dir.join("result.json"), json.as_bytes())?;
        write_atomic(&dir.join("summary.md"), self.summary_md().as_bytes())?;
        Ok(())
    }
}

/// Writes `bytes` to a temporary file next to `path` and renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
