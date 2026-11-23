// SPDX-License-Identifier: MPL-2.0
//! The soak workload: one `NetServer` and N `NetClient`s of the sample game (plus a hostile peer
//! on the `hostile` profile) on one [`ImpairedNet`], driven in virtual time.
//!
//! Each client joins in the first seconds, sends one input per server tick (stamped by the
//! engine's own loop, `NetClient::due_input_ticks`, with the lead the server's input feedback
//! steers, ADR-8077; redundancy 15, with
//! small and large command tails, and a heavy cohort that sends 0.5-1 KiB tails every tick for a
//! while), predicts its avatar, sends reliable commands of 0.5-4 KiB and chat lines, leaves and
//! rejoins on a schedule, and reconnects by itself after an involuntary end. The server simulates
//! the inputs that arrived, sends a world blob on join and a snapshot every tick, sized by the
//! transport's send allowance for that client (`NetServer::send_allowance`, the rate controller of
//! spec 120 § 4.2, ADR-8301), and answers every command.
//! Everything is recorded for [`super::report`].
//!
//! The spec 120 § 15.1 profiles add (ADR-8030): each client runs on its own clock
//! ([`LinkSpec::local_ms`]: every `NetClient` call gets the client's local time and its frames fire
//! every [`FRAME_MS`] of local time), a bulk transfer to every client on `+bulk` runs, a peer that
//! floods valid sealed records (`flood`), forged `ConnectRequest`s from many /24s
//! (`forged-handshake`), and the server's address for every peer checked against the addresses
//! its client really held (a move to any other address is a hijack).
//!
//! The server and the clients follow the engine's own policies (ADR-7950 silence policy, ADR-8077
//! input lead); the kit has no copy of either. [`run_with`] replaces them for a test that shows a
//! bar failing on a broken policy ([`Knobs`]).

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use regia_net::capture::{CaptureError, CaptureFile, CaptureRecord, CaptureTap, ReplayFeed};
use regia_net::reconnect::{ReconnectPolicy, ReconnectStep, Reconnector};
use regia_net::status::EndpointTotals;
use regia_net::{
    Channel, ClientConfig, ClientEvent, ClientState, HealthPolicy, NetClient, NetServer, PeerId, SendError,
    ServerConfig, ServerEvent, TimeoutPolicy,
};
use serde::{Deserialize, Serialize};

use super::game::{
    BULK_CHUNK, ENTITY_BYTES, EntityState, HostileGame, MAX_COMMAND_SAMPLE, RawBytes, STEP_UNITS, SoakGame,
    SoakInput, SoakMsg, over_cap_input,
};
use super::impair::{DatagramTrace, EndpointId, ImpairedNet, ImpairedSocket, LinkSpec, SoakRng};
use super::profiles::Profile;

/// Server tick rate, Hz.
pub const TICK_HZ: u64 = 30;
/// Virtual time step of the driver, milliseconds.
pub const STEP_MS: u64 = 2;
/// Client frame interval, milliseconds.
pub const FRAME_MS: u64 = 16;
/// The fixed input lead a game used before the server's input feedback (ADR-8077), ticks ahead of
/// the client's estimate of the server tick: what [`InputLead::Fixed`] runs with when a test
/// switches the feedback off. A run stamps with the engine's steered lead ([`InputLead::Steered`]).
pub const INPUT_LEAD_TICKS: u64 = 2;
/// Input redundancy of every client (DO27's setting).
pub const INPUT_REDUNDANCY: usize = 15;
/// Entities per snapshot part.
pub const PART_ENTITIES: usize = 80;
/// Total replicated entities the game aims for (avatars plus NPCs, at least 64 NPCs).
pub const TARGET_ENTITIES: usize = 140;
/// Bytes of the world blob sent on join.
pub const WORLD_BLOB_BYTES: usize = 16 * 1024;
/// Server address.
pub const SERVER_ADDR: ([u8; 4], u16) = ([10, 0, 0, 1], 7777);
/// A connected client whose inputs stop reaching the server, or that gets no snapshot, for this
/// long while its link is up, is wedged.
pub const WEDGE_MS: u64 = 2_000;
/// A client's input ticks are judged from its first input at the server or this long after it
/// joined, whichever comes first (`missed_input`), milliseconds.
pub const INPUT_GRACE_MS: u64 = 1_000;
/// `forged-handshake`: forged `ConnectRequest`s per second.
pub const FORGED_PER_S: u64 = 100_000;
/// `forged-handshake`: the /24 prefixes the forged requests come from.
pub const FORGED_PREFIXES: u64 = 1_000;
/// `flood`: valid sealed datagrams per second from the flooding peer.
pub const FLOOD_DATAGRAMS_PER_S: u64 = 100_000;

/// Dedicated or listen server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerMode {
    /// The server sits on a clean datacenter link.
    Dedicated,
    /// The server sits behind a home uplink shared by every client (`listen_server_link`); the
    /// host's own player does not cross the network.
    Listen,
}

impl ServerMode {
    /// Lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dedicated => "dedicated",
            Self::Listen => "listen",
        }
    }

    /// Parses a name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dedicated" => Some(Self::Dedicated),
            "listen" => Some(Self::Listen),
            _ => None,
        }
    }
}

/// One run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    /// The network profile.
    pub profile: Profile,
    /// Honest clients.
    pub clients: usize,
    /// Server mode.
    pub mode: ServerMode,
    /// Seed of every random stream.
    pub seed: u64,
    /// Virtual length, milliseconds.
    pub duration_ms: u64,
    /// The datagram path: the in-process network, or the same network over real UDP sockets
    /// (ADR-8454).
    #[serde(default)]
    pub wire: WireMode,
}

impl Scenario {
    /// `profile/N/mode`, plus `/udp` on the real UDP wire.
    pub fn label(&self) -> String {
        let wire = match self.wire {
            WireMode::Virtual => "",
            WireMode::Udp => "/udp",
        };
        format!(
            "{}/{}/{}{wire}",
            self.profile.name,
            self.clients,
            self.mode.as_str()
        )
    }
}

/// The datagram path of a run (ADR-8454).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireMode {
    /// Datagrams move between in-process queues (`ImpairedNet::new`).
    #[default]
    Virtual,
    /// Every delivered datagram also crosses a real UDP socket pair on 127.0.0.1
    /// (`ImpairedNet::with_real_udp`): same impairments, same virtual addresses, so encryption and
    /// the address checks run as usual, plus the system calls and the kernel's copies.
    Udp,
}

impl WireMode {
    /// `virtual` or `udp`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Virtual => "virtual",
            Self::Udp => "udp",
        }
    }

    /// Parses [`Self::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "virtual" => Some(Self::Virtual),
            "udp" => Some(Self::Udp),
            _ => None,
        }
    }
}

/// How a soak client picks the ticks it stamps its inputs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputLead {
    /// The engine's own loop (ADR-8077): `NetClient::due_input_ticks`, every tick once up to the
    /// tick estimate plus the lead steered from the server's input feedback.
    #[default]
    Steered,
    /// Input feedback switched off: this many ticks ahead of the tick estimate whatever the server
    /// reports, with at most three ticks of catch-up after a stall (the kit's loop before
    /// ADR-8077; [`INPUT_LEAD_TICKS`] was its lead).
    Fixed(u64),
}

/// The policies a run's endpoints follow ([`run_with`]). The default is the engine's own, and it
/// is what [`run`] uses. The bars always judge against the engine's defaults
/// (`report::timeout_policy`), so a run with a broken policy here is how a test shows a bar
/// catching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Knobs {
    /// The silence policy of the server and of every client (ADR-7950); the default is
    /// `regia_net::TimeoutPolicy::default()`.
    pub timeouts: TimeoutPolicy,
    /// The clients' input lead.
    pub input_lead: InputLead,
    /// The hostile peer (`hostile` profile) is a full relay as well as a replayer: whatever the
    /// server sends to its replay address it carries on to its victim under the server's address,
    /// so the server's path probe (ADR-8299) reaches the victim, the victim acknowledges it, and
    /// the next replayed record validates the attacker's address. Path validation does not stop
    /// this (ADR-8299 Consequences: "a full relay can still carry the probe and become the
    /// path"); it is the hijack `no_hijack` must still report (ADR-8424). `false` (the default)
    /// is the profile's own attacker, which only replays.
    pub hostile_relay: bool,
    /// The transport's rate controller and pacer (spec 120 § 4.2, ADR-8301) switched off on the
    /// server and on every client (`LinkConfig::rate.enabled = false`): every due packet leaves
    /// at once, as v0.5 sent. `false` (the default) runs the engine's controller.
    /// `rate_vs_bottleneck` on congested keeps a run it fails on this way (ADR-8424, ADR-8487).
    pub rate_control_off: bool,
    /// One client sends no input for a while (it stays connected, and still sends its commands
    /// and chat): the client `no_wedged_client` must report as `server_no_input` (ADR-8887).
    /// `None` (the default): every client sends every due input.
    pub silent_inputs: Option<Silence>,
    /// The server sends one client no snapshot for a while (the connection and every other
    /// message stay): the client `no_wedged_client` must report as `no_snapshot` (ADR-8887).
    /// `None` (the default): every client gets every snapshot the budget allows.
    pub silent_snapshots: Option<Silence>,
    /// How a client reconnects by itself after a timeout while it holds a resume ticket
    /// (ADR-8352, ADR-10093); the default is `regia_net::ReconnectPolicy::default()`, what a
    /// `NetLink` or `NetSession` client runs. `ReconnectPolicy::off()` is the client that gives up
    /// at the timeout and joins afresh: `interrupted_once` and `resume_same_peer` keep a run they
    /// fail on this way.
    pub reconnect: ReconnectPolicy,
    /// Replaces the profile's server resume grace (`Profile::server_resume_grace_ms`),
    /// milliseconds; `None` (the default) keeps the profile's.
    pub server_resume_grace_ms: Option<u64>,
    /// The rate controller's start rate on the server and on every client, bytes per second
    /// (`LinkConfig::rate.start_bytes_per_second`; the first burst follows it, ADR-10145). `None`
    /// (the default) keeps the engine's 256 KiB/s. A project with players on thin links sets it
    /// in `config/net.ron`; the cautious-start runs of ADR-10145 set 32 KiB/s here.
    pub start_bytes_per_second: Option<u32>,
    /// The first flight of ADR-10156 on the server and on every client
    /// (`LinkConfig::rate.first_flight`): until a path has answered once its sender offers it the
    /// floor's rate and a first burst of eight packets, and the startup begins at what the first
    /// round delivered. `None` (the default) keeps the engine's default, off in v0.7 (ADR-10231):
    /// the blind initial window at the start rate; `Some(true)` switches the first flight on.
    pub first_flight: Option<bool>,
}

/// One client's traffic of one kind withheld from `from_ms` until `until_ms`
/// ([`Knobs::silent_inputs`], [`Knobs::silent_snapshots`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Silence {
    /// The client index.
    pub client: usize,
    /// Start of the silence, virtual milliseconds.
    pub from_ms: u64,
    /// End of the silence (exclusive), virtual milliseconds; `u64::MAX` for the rest of the run.
    pub until_ms: u64,
}

impl Silence {
    /// True when the silence covers `client` at `now_ms`.
    pub fn covers(&self, client: usize, now_ms: u64) -> bool {
        self.client == client && now_ms >= self.from_ms && now_ms < self.until_ms
    }
}

/// One end of a connection, seen by either side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndRecord {
    /// Client index.
    pub client: usize,
    /// Virtual time, milliseconds.
    pub at_ms: u64,
    /// `client` or `server`.
    pub side: String,
    /// The end code (`ClientEnd::as_str` / `DisconnectReason::as_str`).
    pub cause: String,
    /// The peer id of the connection that ended (0 when it never had one).
    pub peer: u64,
    /// The workload asked for it (a scripted leave).
    pub voluntary: bool,
}

/// One rejoin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconnect {
    /// Client index.
    pub client: usize,
    /// `scripted` (after a planned leave) or `involuntary` (after a disconnect).
    pub kind: String,
    /// When the client started reconnecting.
    pub start_ms: u64,
    /// When its link was usable again (after any outage covering the start).
    pub link_up_ms: u64,
    /// When it was connected again, if it was.
    pub connected_ms: Option<u64>,
}

/// One wedge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wedge {
    /// Client index.
    pub client: usize,
    /// `server_no_input`, `no_snapshot` or `input_send_failing`.
    pub kind: String,
    /// Gap start.
    pub start_ms: u64,
    /// Gap length.
    pub length_ms: u64,
    /// The input send errors during the gap, by kind.
    pub send_errors: BTreeMap<String, u64>,
}

/// A fixed-bucket histogram of small non-negative values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Histogram {
    /// Count per value; the last bucket holds everything at or above it.
    pub buckets: Vec<u64>,
    /// Largest value seen.
    pub max: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new(64)
    }
}

impl Histogram {
    /// A histogram with `n` buckets.
    pub fn new(n: usize) -> Self {
        Self {
            buckets: vec![0; n.max(1)],
            max: 0,
        }
    }

    /// Adds one value.
    pub fn add(&mut self, v: u64) {
        let last = self.buckets.len() - 1;
        self.buckets[(v as usize).min(last)] += 1;
        self.max = self.max.max(v);
    }

    /// Adds every count of `other` (same bucket count).
    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.buckets.iter_mut().zip(&other.buckets) {
            *a += b;
        }
        self.max = self.max.max(other.max);
    }

    /// Values counted.
    pub fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// The value at quantile `q` in `[0, 1]` (bucket resolution; 0 when empty).
    pub fn quantile(&self, q: f64) -> u64 {
        let total = self.count();
        if total == 0 {
            return 0;
        }
        let rank = ((total as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (v, c) in self.buckets.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return v as u64;
            }
        }
        self.max
    }
}

/// Thousandths of a tick in one millisecond: the unit of [`StalenessParts`], exact for whole
/// milliseconds at any tick rate (a tick is 1,000).
pub const MILLITICKS_PER_MS: i64 = TICK_HZ as i64;

/// Where a client's snapshot staleness comes from (ADR-10096): the sums of every sample taken with
/// the link up, in thousandths of a tick ([`MILLITICKS_PER_MS`] per millisecond). For every
/// sample `total = path + transit + hold + rounding + estimate`, so the sums add up the same way.
///
/// The harness knows what the client cannot: when the server sent each snapshot, the tick the
/// server is really on, and the latency its links add.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalenessParts {
    /// Samples summed.
    pub samples: u64,
    /// The bar's own samples (`staleness_ticks_link_up`): the client's estimate of the server
    /// tick minus the tick of its newest snapshot.
    pub total: i64,
    /// The one-way latency of the path toward the client in force at the sample (the server's
    /// uplink plus the client's downlink, without jitter or queueing): no sender can be faster.
    pub path: i64,
    /// The newest snapshot's way from the server's send to the client's frame that read it,
    /// beyond `path`: jitter, queueing, anything the sender held it for, and the wait for the
    /// client's next frame.
    pub transit: i64,
    /// How long the newest snapshot has been the newest: the send interval, and every snapshot
    /// lost or late after it.
    pub hold: i64,
    /// Counting in whole ticks: the server is on tick `t + 1` from the moment it sends the
    /// snapshot of tick `t` (between 0 and 1 tick, half a tick on average).
    pub rounding: i64,
    /// The client's tick estimate minus the tick the server is really on (negative: the estimate
    /// runs behind).
    pub estimate: i64,
    /// The fewest whole ticks of `path` at any sample ([`ClientRecord::staleness_beyond_path_link_up`]
    /// takes them off).
    pub path_ticks_min: u64,
    /// The most whole ticks of `path` at any sample.
    pub path_ticks_max: u64,
}

impl StalenessParts {
    /// Adds every sum of `other`.
    pub fn merge(&mut self, other: &StalenessParts) {
        if other.samples == 0 {
            return;
        }
        if self.samples == 0 {
            self.path_ticks_min = other.path_ticks_min;
            self.path_ticks_max = other.path_ticks_max;
        } else {
            self.path_ticks_min = self.path_ticks_min.min(other.path_ticks_min);
            self.path_ticks_max = self.path_ticks_max.max(other.path_ticks_max);
        }
        self.samples += other.samples;
        self.total += other.total;
        self.path += other.path;
        self.transit += other.transit;
        self.hold += other.hold;
        self.rounding += other.rounding;
        self.estimate += other.estimate;
    }

    /// The mean of one sum, ticks (0 without samples).
    pub fn mean_ticks(&self, sum: i64) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            sum as f64 / 1000.0 / self.samples as f64
        }
    }
}

/// What one client recorded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientRecord {
    /// Client index.
    pub client: usize,
    /// Joined at least once.
    pub joined: bool,
    /// Connected intervals `(peer id, from, to)`; `to` is the run end for an open one.
    pub connected: Vec<(u64, u64, u64)>,
    /// Inputs handed to `send_input` successfully.
    pub inputs_sent: u64,
    /// `send_input` errors by kind.
    pub input_send_errors: BTreeMap<String, u64>,
    /// Inputs stamped for a tick the server had already reached by the client's estimate.
    pub inputs_stamped_behind: u64,
    /// Reliable commands sent / acknowledged.
    pub commands_sent: u64,
    /// Commands acknowledged.
    pub commands_acked: u64,
    /// Reliable send errors by kind.
    pub reliable_send_errors: BTreeMap<String, u64>,
    /// Snapshot parts received.
    pub snapshot_parts: u64,
    /// Corrections (server disagreed with the prediction).
    pub corrections: u64,
    /// Snapshots compared with a prediction.
    pub corrections_checked: u64,
    /// Correction size, steps (Manhattan distance / step).
    pub correction_steps: Histogram,
    /// Rollback depth of each correction, ticks.
    pub rollback_ticks: Histogram,
    /// Snapshot staleness sampled every frame, ticks.
    pub staleness_ticks: Histogram,
    /// Largest staleness sample of each second of the run, ticks (index = second).
    pub staleness_max_per_s: Vec<u16>,
    /// Command round trips, in frames of 16 ms.
    pub command_rtt_frames: Histogram,
    /// Join or reconnect attempts that timed out.
    pub connect_timeouts: u64,
    /// Join or reconnect attempts the server refused, by code.
    pub connect_refusals: BTreeMap<String, u64>,
    /// The client's sub-profile (its own profile, or its entry of a mixed one).
    #[serde(default)]
    pub profile: String,
    /// Snapshot staleness sampled every frame while the client's link is up (outages excluded),
    /// ticks.
    #[serde(default)]
    pub staleness_ticks_link_up: Histogram,
    /// Every join or rejoin attempt: `(start ms, connected ms)`.
    #[serde(default)]
    pub join_attempts: Vec<(u64, Option<u64>)>,
    /// Every reliable command: `(sent ms, acknowledged ms)`.
    #[serde(default)]
    pub command_log: Vec<(u64, Option<u64>)>,
    /// `Interrupted` / `Restored` events the client saw: `(ms, "interrupted" | "restored")`. The
    /// engine raises none yet (spec 120 § 10.2, lane net-timeout-policy); the workload records
    /// them once it does.
    #[serde(default)]
    pub interruptions: Vec<(u64, String)>,
    /// When the client left on purpose (the scripted leave), milliseconds. A goodbye inside an
    /// outage reaches nobody, so no end record says it; `interrupted_once` reads it here.
    #[serde(default)]
    pub leaves: Vec<u64>,
    /// Automatic reconnect attempts started after a timeout (ADR-8352, ADR-10093): fresh
    /// handshakes that presented the resume ticket.
    #[serde(default)]
    pub reconnect_attempts: u64,
    /// Reconnect timelines that reached their window's end without an accept; the client joined
    /// afresh after each.
    #[serde(default)]
    pub reconnects_given_up: u64,
    /// Bulk chunks received.
    #[serde(default)]
    pub bulk_chunks: u32,
    /// When the whole bulk transfer had arrived.
    #[serde(default)]
    pub bulk_done_ms: Option<u64>,
    /// `staleness_ticks_link_up` with the path taken off each sample (ADR-10096): the sample
    /// minus the whole ticks of the one-way latency toward the client in force at that moment
    /// (never below 0). What is left is what the sender, the client and the estimate add.
    #[serde(default)]
    pub staleness_beyond_path_link_up: Histogram,
    /// Where the link-up samples come from (ADR-10096).
    #[serde(default)]
    pub staleness_parts: StalenessParts,
    /// Where the staleness of the link-up samples went, stage by stage (ADR-10145).
    #[serde(default)]
    pub staleness_stages: StalenessStages,
    /// Snapshot staleness of the link-up samples taken [`STALENESS_SETTLE_MS`] or more after the
    /// connect they belong to, ticks: the run without its join transients (ADR-10145; a number
    /// for the report, no bar reads it).
    #[serde(default)]
    pub staleness_ticks_settled: Histogram,
    /// How long after each connect the join's world blob had arrived, milliseconds (one entry per
    /// join that got it): what a slower start costs a join (ADR-10145).
    #[serde(default)]
    pub world_after_connect_ms: Vec<u64>,
    /// Every join snapshot (the world blob of a join or rejoin) that arrived: `(connected ms,
    /// arrived ms)`. A connection that ended before its blob came leaves no entry (ADR-10125).
    #[serde(default)]
    pub join_snapshots: Vec<(u64, u64)>,
}

/// The stages a snapshot's age is split into, in the order of [`StalenessStages::stage_us`]
/// (ADR-10145). For the newest snapshot a client holds when it samples its staleness:
///
/// - `sender_hold`: from the server tick that made the snapshot to the moment its datagram left
///   the server's socket (the send queue and the pacer);
/// - `queue`: what that datagram waited in the bottleneck queues of its route;
/// - `serialize`: its own transmission time at those bottlenecks;
/// - `path`: latency, jitter and any reordering hold;
/// - `receiver_hold`: from its arrival in the client's socket to the frame that handed the
///   snapshot to the game;
/// - `wait`: from that frame to the sample (the time no newer snapshot arrived in: the snapshot
///   interval, and every snapshot lost, expired in the sender or withheld);
/// - `estimate`: the bar's count (the client's tick estimate minus the snapshot's tick, in whole
///   ticks) minus the snapshot's true age, the sum of the six stages above: the tick label (a
///   snapshot of tick `t` is made when the server is at `t + 1`), whole-tick counting and the
///   error of the client's estimate of the server tick. Negative when the estimate runs behind.
pub const STALENESS_STAGES: [&str; 7] = [
    "sender_hold",
    "queue",
    "serialize",
    "path",
    "receiver_hold",
    "wait",
    "estimate",
];

/// Staleness samples this long after their connect are "settled": the join's world blob and the
/// rate controller's startup are over (the settle time of the `rate_vs_bottleneck` bar,
/// `report::RATE_SETTLE_MS`).
pub const STALENESS_SETTLE_MS: u64 = 5_000;

/// Where a client's staleness went: for every staleness value, the samples that read it and the
/// summed time of each stage of [`STALENESS_STAGES`] (ADR-10145). The seven stages of a sample
/// add up to its staleness exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalenessStages {
    /// Traced samples by staleness, ticks (index = staleness; the last bucket,
    /// [`Self::BUCKETS`]` - 1`, holds every larger one).
    pub samples: Vec<u64>,
    /// Summed stage times of those samples, microseconds: `[staleness][stage]`.
    pub stage_us: Vec<[i64; 7]>,
    /// Link-up samples whose newest snapshot could not be traced to a datagram (the real UDP
    /// wire, a capture replay).
    pub untraced: u64,
}

impl StalenessStages {
    /// Staleness values kept apart.
    pub const BUCKETS: usize = 256;

    /// Adds one sample of `stale` ticks with its stage times in microseconds.
    pub fn add(&mut self, stale: u64, stages: [i64; 7]) {
        if self.samples.len() < Self::BUCKETS {
            self.samples.resize(Self::BUCKETS, 0);
            self.stage_us.resize(Self::BUCKETS, [0; 7]);
        }
        let i = (stale as usize).min(Self::BUCKETS - 1);
        self.samples[i] += 1;
        for (sum, v) in self.stage_us[i].iter_mut().zip(stages) {
            *sum += v;
        }
    }

    /// Adds another client's samples.
    pub fn merge(&mut self, other: &StalenessStages) {
        self.untraced += other.untraced;
        if other.samples.is_empty() {
            return;
        }
        if self.samples.len() < Self::BUCKETS {
            self.samples.resize(Self::BUCKETS, 0);
            self.stage_us.resize(Self::BUCKETS, [0; 7]);
        }
        for (i, (&n, sums)) in other.samples.iter().zip(&other.stage_us).enumerate() {
            let i = i.min(Self::BUCKETS - 1);
            self.samples[i] += n;
            for (sum, v) in self.stage_us[i].iter_mut().zip(sums) {
                *sum += v;
            }
        }
    }

    /// The traced samples of at least `from` ticks and the mean time of each stage over them, in
    /// ticks of `tick_hz`.
    pub fn mean_ticks(&self, from: u64, tick_hz: u64) -> (u64, [f64; 7]) {
        let (mut n, mut sums) = (0u64, [0i64; 7]);
        for (i, (&count, stage)) in self.samples.iter().zip(&self.stage_us).enumerate() {
            if (i as u64) < from {
                continue;
            }
            n += count;
            for (sum, v) in sums.iter_mut().zip(stage) {
                *sum += v;
            }
        }
        let tick_us = 1_000_000.0 / tick_hz.max(1) as f64;
        (
            n,
            sums.map(|sum| {
                if n == 0 {
                    0.0
                } else {
                    sum as f64 / n as f64 / tick_us
                }
            }),
        )
    }
}

/// The newest snapshot a client holds, traced to the datagram that carried it (ADR-10145).
#[derive(Debug, Clone, Copy)]
struct Carried {
    /// The snapshot's tick.
    tick: u64,
    /// When the server made it, microseconds.
    made_us: u64,
    /// Its datagram.
    trace: DatagramTrace,
    /// The frame that handed it to the game, microseconds.
    handed_us: u64,
}

impl Carried {
    /// The stage times of a sample of `stale` ticks taken at `now_us` ([`STALENESS_STAGES`]).
    fn stages(&self, stale: u64, now_us: u64) -> [i64; 7] {
        let t = &self.trace;
        let d = |a: u64, b: u64| a as i64 - b as i64;
        let age = d(now_us, self.made_us);
        [
            d(t.sent_us, self.made_us),
            t.queue_us as i64,
            t.serialize_us as i64,
            d(t.arrive_us, t.sent_us) - t.queue_us as i64 - t.serialize_us as i64,
            d(self.handed_us, t.arrive_us),
            d(now_us, self.handed_us),
            (stale.min(1 << 32) * 1_000_000 / TICK_HZ) as i64 - age,
        ]
    }
}

/// What the hostile peer did and how the server answered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostileSession {
    /// `over_cap_input`, `malformed_messages` or `flood`.
    pub kind: String,
    /// Window start.
    pub start_ms: u64,
    /// Window end.
    pub end_ms: u64,
    /// When it connected.
    pub connected_ms: Option<u64>,
    /// First hostile act.
    pub first_act_ms: Option<u64>,
    /// When the server ended it (as the hostile client saw it).
    pub dropped_ms: Option<u64>,
    /// The end code the hostile client saw.
    pub end_cause: Option<String>,
    /// Hostile sends.
    pub acts: u64,
}

/// The hostile peer's record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostileRecord {
    /// Connected sessions.
    pub sessions: Vec<HostileSession>,
    /// Replayed datagrams of a victim, sent from the hostile address.
    pub replays_sent: u64,
    /// Garbage datagrams from an unconnected address.
    pub garbage_sent: u64,
    /// Oversized datagrams (over 2 KiB) from an unconnected address.
    pub oversized_sent: u64,
}

/// A change of the server's address for one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressMove {
    /// Client index.
    pub client: usize,
    /// Peer id.
    pub peer: u64,
    /// When the server's address for the peer changed, milliseconds.
    pub at_ms: u64,
    /// The new address is one the client never held (the peer's traffic now goes elsewhere).
    pub foreign: bool,
}

/// The server's record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerRecord {
    /// Wall time of each server iteration that ran a tick, microseconds.
    pub tick_us: Vec<u32>,
    /// Ticks simulated.
    pub ticks: u64,
    /// Per client: ticks simulated while it was registered and after its first input, with input.
    pub ticks_with_input: BTreeMap<usize, u64>,
    /// Per client: same, without input (lost or late), outages excluded.
    pub ticks_missed: BTreeMap<usize, u64>,
    /// Input gaps `(client, peer, start, end)` at least [`WEDGE_MS`] long, outages included: closed
    /// by the next input, the peer's removal or the end of the run (ADR-8887). `report::wedges`
    /// judges the parts the client's link was up and the client connected as that peer.
    pub input_gaps: Vec<(usize, u64, u64, u64)>,
    /// Snapshot sends refused, by kind.
    pub snapshot_send_errors: BTreeMap<String, u64>,
    /// Smallest entity budget any client got after the first 5 s, outside its sender's first
    /// flight (ADR-10200).
    pub min_entity_budget: Option<usize>,
    /// Smallest entity budget a client got after the first 5 s during its sender's first flight
    /// (a rejoin's, ADR-10156): the essential floor's share, which `random_loss_kept` does not
    /// read (ADR-10200). `None` when no first flight was seen then.
    #[serde(default)]
    pub min_flight_entity_budget: Option<usize>,
    /// Server endpoint totals at the end.
    pub totals: EndpointTotals,
    /// Every change of the server's address for a client's peer.
    #[serde(default)]
    pub address_moves: Vec<AddressMove>,
    /// When the bulk transfer started (`+bulk` runs).
    #[serde(default)]
    pub bulk_start_ms: Option<u64>,
    /// Entities in the world the server replicates (the full snapshot budget).
    #[serde(default)]
    pub world_entities: usize,
    /// Per client: ticks judged for input, from its first input or [`INPUT_GRACE_MS`] after it
    /// joined, link up (unlike `ticks_with_input`, a client whose input never arrives counts).
    #[serde(default)]
    pub input_ticks_judged: BTreeMap<usize, u64>,
    /// Per client: of those, ticks that had its input.
    #[serde(default)]
    pub input_ticks_arrived: BTreeMap<usize, u64>,
    /// Allocations the server made in each iteration that ran a tick (same order as `tick_us`),
    /// the impairment network's own copies excluded; empty when the binary installed no counter
    /// ([`super::probe`], ADR-8454).
    #[serde(default)]
    pub tick_allocs: Vec<u32>,
    /// Wall time of every server iteration together (receive, ticks, flush), microseconds: the
    /// server's CPU time for the run.
    #[serde(default)]
    pub busy_us: u64,
    /// The phases of each iteration that ran a tick, microseconds (same order as `tick_us`):
    /// [`TICK_PHASES`] (receive, events, harness checks, tick, flush).
    #[serde(default)]
    pub tick_phases_us: Vec<[u32; 5]>,
}

/// The phases of a server iteration that `ServerRecord::tick_phases_us` times: `receive`
/// (`NetServer::receive`), `events` (the joins, leaves and messages it raised, with the world blob
/// and command answers they send), `harness` (the soak's own address checks and bulk pump),
/// `tick` (`advance_tick`, the sample game's simulation and a snapshot for every client) and
/// `flush` (`NetServer::flush`: packing, sealing and sending).
pub const TICK_PHASES: [&str; 5] = ["receive", "events", "harness", "tick", "flush"];

/// Everything a run produced, before evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// The scenario.
    pub scenario: Scenario,
    /// Client links (index = client).
    pub links: Vec<LinkSpec>,
    /// Ends seen by either side.
    pub ends: Vec<EndRecord>,
    /// Rejoins.
    pub reconnects: Vec<Reconnect>,
    /// Client wedges detected on the client side (`no_snapshot`, `input_send_failing`).
    pub client_gaps: Vec<Wedge>,
    /// Per client.
    pub clients: Vec<ClientRecord>,
    /// The server.
    pub server: ServerRecord,
    /// The hostile peer.
    pub hostile: Option<HostileRecord>,
    /// Per client network counters.
    pub client_net: Vec<super::impair::EndpointStats>,
    /// The server's network counters.
    pub server_net: super::impair::EndpointStats,
    /// Wall time of the run, milliseconds.
    pub wall_ms: u64,
    /// Forged `ConnectRequest`s sent (`forged-handshake`).
    #[serde(default)]
    pub forged_sent: u64,
    /// The real UDP wire's counters (`None` on the virtual wire).
    #[serde(default)]
    pub wire: Option<super::impair::WireStats>,
    /// The process's resident set at the end of the run and its peak, KiB (Linux
    /// `/proc/self/status` `VmRSS` / `VmHWM`; `None` elsewhere). The whole process: server,
    /// clients and network, and every run before this one in the same process.
    #[serde(default)]
    pub rss_kib: Option<(u64, u64)>,
}

impl RunRecord {
    /// Clears what depends on the host rather than the seed: wall-clock times (`tick_us`,
    /// `tick_phases_us`, `busy_us`, `wall_ms`) and the process's RSS. What is left is the same for
    /// the same scenario on every machine.
    pub fn clear_host_measurements(&mut self) {
        self.server.tick_us.clear();
        self.server.tick_phases_us.clear();
        self.server.busy_us = 0;
        self.wall_ms = 0;
        self.rss_kib = None;
    }
}

/// Bytes one snapshot part costs besides its entities: the message fields, the record and its share
/// of the packet and seal headers.
const PART_OVERHEAD_BYTES: usize = 64;

/// Entities one tick's snapshot may carry at `allowance` bytes per second (the transport's rate
/// controller, spec 120 § 4.2 item 9): the game sizes its sends to it, as replication does with
/// `min(project rate, allowance)`. At least 8, at most the world.
pub fn entity_budget(allowance: Option<u32>, world: usize) -> usize {
    let Some(allowance) = allowance else {
        return world;
    };
    let per_tick = allowance as usize / TICK_HZ as usize;
    let mut n = per_tick / ENTITY_BYTES;
    // Every part of PART_ENTITIES costs its overhead.
    while n > 0 && n * ENTITY_BYTES + n.div_ceil(PART_ENTITIES) * PART_OVERHEAD_BYTES > per_tick {
        n -= 1;
    }
    n.clamp(8.min(world), world)
}

/// The `+bulk` transfer rides the bulk stream (spec 120 § 5.1, ADR-8302).
const BULK_CHANNEL: Channel = Channel::Stream(regia_net::connection::StreamId::BULK);
/// The join's world blob is a join snapshot: it rides the replication control stream, a
/// foreground stream, so a join never waits behind state frames (spec 120 § 5.1, ADR-8624).
const JOIN_CHANNEL: Channel = Channel::Stream(regia_net::connection::StreamId::REPLICATION);
/// Chat rides the chat stream.
const CHAT_CHANNEL: Channel = Channel::Stream(regia_net::connection::StreamId::CHAT);

fn send_error_kind(e: &SendError) -> &'static str {
    match e {
        SendError::TooLarge { .. } => "too_large",
        SendError::Backpressure(_) => "backpressure",
        SendError::NotConnected => "not_connected",
        SendError::RateLimited => "rate_limited",
        SendError::UnknownStream(_) => "unknown_stream",
    }
}

fn client_addr(i: usize) -> SocketAddr {
    SocketAddr::from(([10, 1 + (i / 250) as u8, 0, (i % 250) as u8 + 1], 40_000))
}

fn server_addr() -> SocketAddr {
    SocketAddr::from(SERVER_ADDR)
}

fn client_config(
    name: String,
    timeouts: TimeoutPolicy,
    mtu_probing: bool,
    rate_control_off: bool,
    start_bytes_per_second: Option<u32>,
    first_flight: Option<bool>,
) -> ClientConfig {
    ClientConfig {
        name,
        input_redundancy: INPUT_REDUNDANCY,
        link: soak_link_config(
            mtu_probing,
            rate_control_off,
            start_bytes_per_second,
            first_flight,
        ),
        ..ClientConfig::default()
    }
    .with_timeouts(timeouts)
}

/// The transport settings of every soak endpoint: the profile's path MTU probing and, unless a
/// [`Knobs::rate_control_off`] run switches it off, the engine's rate controller (with the
/// start rate and the first flight a run's [`Knobs`] ask for).
fn soak_link_config(
    mtu_probing: bool,
    rate_control_off: bool,
    start_bytes_per_second: Option<u32>,
    first_flight: Option<bool>,
) -> regia_net::connection::LinkConfig {
    let mut link = regia_net::connection::LinkConfig {
        mtu_probing,
        ..regia_net::connection::LinkConfig::default()
    };
    if rate_control_off {
        link.rate.enabled = false;
    }
    if let Some(start) = start_bytes_per_second {
        link.rate.start_bytes_per_second = start;
    }
    if let Some(on) = first_flight {
        link.rate.first_flight = on;
    }
    link
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Not started; join at this time.
    Waiting(u64),
    /// Connecting (the client exists).
    Connecting,
    /// Connected.
    Connected,
    /// Left on purpose; rejoin at this time.
    Left(u64),
    /// Timed out holding a resume ticket: the reconnect timeline runs (ADR-8352, ADR-10093).
    Reconnecting,
}

struct Client {
    idx: usize,
    ep: EndpointId,
    link: LinkSpec,
    net: ImpairedNet,
    client: Option<NetClient<SoakGame, ImpairedSocket>>,
    phase: Phase,
    /// Next frame, in the client's local time.
    next_frame_local: u64,
    rng: SoakRng,
    rec: ClientRecord,
    /// The open reconnect, an index into the run's reconnect list.
    open_reconnect: Option<usize>,
    last_input_tick: Option<u64>,
    /// `(tick, x, y)` predicted after each tick, oldest first.
    predicted: VecDeque<(u64, i32, i32)>,
    newest_snapshot_tick: Option<u64>,
    /// When the frame that read the newest snapshot ran (virtual time).
    newest_snapshot_seen_ms: u64,
    /// The newest snapshot, traced (ADR-10145); `None` when it could not be.
    carried: Option<Carried>,
    /// The traces of the datagrams this frame read.
    frame_traces: Vec<DatagramTrace>,
    last_snapshot_ms: u64,
    send_fail_since: Option<u64>,
    send_fail_kinds: BTreeMap<String, u64>,
    next_command_ms: u64,
    next_command_id: u32,
    /// Open commands: id -> (sent ms, index in the command log).
    pending_commands: BTreeMap<u32, (u64, usize)>,
    /// The profile turns path MTU probing on (ADR-8300).
    mtu_probing: bool,
    chat_seq: u32,
    leave_at: Option<u64>,
    /// The resume ticket the scripted leave took with it, presented by the rejoin (ADR-8733);
    /// `None` when the server keeps no tickets.
    rejoin_ticket: Option<[u8; regia_net::handshake::RESUME_TICKET_BYTES]>,
    /// The resumption credential of the same accept (wire 6, ADR-8902): with it the rejoin opens
    /// with a one-round-trip `ResumeInit`. `None` when the server keeps no tickets.
    rejoin_credential: Option<regia_net::crypto::ResumeCredential>,
    /// The resume ticket of the live connection's accept (`None` when the server keeps none): what
    /// an automatic reconnect presents after a timeout.
    ticket: Option<[u8; regia_net::handshake::RESUME_TICKET_BYTES]>,
    /// The reconnect timeline of the outage that timed the connection out, in the client's local
    /// time (ADR-8352): the schedule a `NetLink` client runs.
    reconnecting: Option<Reconnector>,
    heavy: Option<(u64, u64)>,
    mv: (i8, i8),
    knobs: Knobs,
    /// Client capture (ADR-8221): every `NetClient` this client makes writes to it.
    capture: Option<CaptureTap>,
    /// Replay: the captured sessions, one per connect, oldest first.
    replay: Option<VecDeque<Vec<CaptureRecord>>>,
    /// Chained digest of the client's output after each frame (corrections, prediction, snapshots).
    digest: u64,
    /// Chained digest of every message the client was handed (what it shows).
    seen: u64,
}

/// FNV-1a over 64-bit words: a digest that stays the same across builds and Rust versions.
#[derive(Debug, Clone, Copy)]
pub struct Fnv64(u64);

impl Fnv64 {
    /// Starts from `seed` (0 starts from the FNV offset basis).
    pub fn new(seed: u64) -> Self {
        Self(0xcbf2_9ce4_8422_2325 ^ seed)
    }

    /// Folds in bytes (their length first).
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u64(b.len() as u64);
        for &x in b {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self
    }

    /// Folds in one word.
    pub fn u64(&mut self, v: u64) -> &mut Self {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self
    }

    /// The digest.
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Client {
    fn new(idx: usize, ep: EndpointId, link: LinkSpec, net: ImpairedNet, s: &Scenario, knobs: Knobs) -> Self {
        net.trace_reads(ep);
        let join_spacing = if s.clients > 64 {
            30
        } else {
            2_000 / s.clients.max(1) as u64
        };
        let d = s.duration_ms;
        let frame_offset = (idx as u64 * 6) % FRAME_MS;
        Self {
            idx,
            ep,
            link,
            net,
            client: None,
            phase: Phase::Waiting(200 + idx as u64 * join_spacing),
            next_frame_local: (FRAME_MS - frame_offset) % FRAME_MS,
            rng: SoakRng::derive(s.seed, 1_000 + idx as u64),
            rec: ClientRecord {
                client: idx,
                correction_steps: Histogram::new(64),
                rollback_ticks: Histogram::new(64),
                staleness_ticks: Histogram::new(256),
                command_rtt_frames: Histogram::new(512),
                staleness_ticks_link_up: Histogram::new(256),
                staleness_beyond_path_link_up: Histogram::new(256),
                staleness_ticks_settled: Histogram::new(256),
                profile: s.profile.client_profile(idx),
                ..ClientRecord::default()
            },
            open_reconnect: None,
            last_input_tick: None,
            predicted: VecDeque::new(),
            newest_snapshot_tick: None,
            newest_snapshot_seen_ms: 0,
            carried: None,
            frame_traces: Vec::new(),
            last_snapshot_ms: 0,
            send_fail_since: None,
            send_fail_kinds: BTreeMap::new(),
            next_command_ms: 0,
            next_command_id: 1,
            pending_commands: BTreeMap::new(),
            mtu_probing: s.profile.mtu_probing,
            chat_seq: 0,
            // Every eighth client leaves at 30 % of the run and rejoins two seconds later.
            leave_at: (idx % 8 == 5).then_some(d * 3 / 10),
            rejoin_ticket: None,
            rejoin_credential: None,
            ticket: None,
            reconnecting: None,
            // Every eighth client sends a 0.5-1 KiB command tail on every tick from 20 % to 30 %.
            heavy: (idx % 8 == 2).then_some((d / 5, d * 3 / 10)),
            mv: (1, 0),
            knobs,
            capture: None,
            replay: None,
            digest: 0,
            seen: 0,
        }
    }

    /// Folds this frame's output into the chained digest: the frame time, the correction counts,
    /// the newest prediction and the sum of every kept one (a correction shifts them all), the
    /// newest snapshot, inputs and commands.
    fn fold_digest(&mut self, now: u64) {
        let (sx, sy) = self.predicted.iter().fold((0i64, 0i64), |(a, b), &(_, x, y)| {
            (a + i64::from(x), b + i64::from(y))
        });
        let newest = self.predicted.back().copied().unwrap_or((u64::MAX, 0, 0));
        let phase = match self.phase {
            Phase::Waiting(_) => 0,
            Phase::Connecting => 1,
            Phase::Connected => 2,
            Phase::Left(_) => 3,
            Phase::Reconnecting => 4,
        };
        let r = &self.rec;
        self.digest = Fnv64::new(self.digest)
            .u64(now)
            .u64(phase)
            .u64(r.corrections)
            .u64(r.corrections_checked)
            .u64(r.snapshot_parts)
            .u64(r.inputs_sent)
            .u64(r.commands_sent)
            .u64(r.commands_acked)
            .u64(r.inputs_stamped_behind)
            .u64(newest.0)
            .u64(newest.1 as u64)
            .u64(newest.2 as u64)
            .u64(sx as u64)
            .u64(sy as u64)
            .u64(self.predicted.len() as u64)
            .u64(self.newest_snapshot_tick.unwrap_or(u64::MAX))
            .u64(self.last_input_tick.unwrap_or(u64::MAX))
            .u64(self.seen)
            .finish();
    }

    /// The client's local clock at virtual time `now`.
    fn local(&self, now: u64) -> u64 {
        self.link.local_ms(now)
    }

    /// True when a frame is due at virtual time `now` (every [`FRAME_MS`] of local time); advances
    /// the frame clock.
    fn frame_due(&mut self, now: u64) -> bool {
        let local = self.local(now);
        if local < self.next_frame_local {
            return false;
        }
        while self.next_frame_local <= local {
            self.next_frame_local += FRAME_MS;
        }
        true
    }

    fn connect(&mut self, now: u64) {
        // A scripted rejoin comes back with the ticket its leave took, the way a game does, on
        // every attempt until one connects, and with the resumption credential of the same accept
        // (wire 6, ADR-8902): a server that keeps tickets resumes it in one round trip.
        if self.open(now, self.rejoin_ticket, self.rejoin_credential.clone(), None) {
            self.phase = Phase::Connecting;
        } else {
            self.phase = Phase::Waiting(now + 1_000);
        }
    }

    /// Starts one handshake on a new socket, presenting `ticket` (and the wire 6 resumption
    /// `credential`, when the caller has one; replayed from the capture in a replay run); `connect_timeout_ms` replaces
    /// the client's own handshake deadline (a reconnect attempt lives until its timeline's
    /// window ends, ADR-8352). False when the client could not be made.
    fn open(
        &mut self,
        now: u64,
        ticket: Option<[u8; regia_net::handshake::RESUME_TICKET_BYTES]>,
        credential: Option<regia_net::crypto::ResumeCredential>,
        connect_timeout_ms: Option<u64>,
    ) -> bool {
        // A new socket: a new public port, like a game that rebinds to reconnect.
        if self.rec.joined || self.rec.connect_timeouts > 0 {
            self.net.rebind_now(self.ep);
        }
        let socket = self.net.socket(self.ep);
        let mut config = client_config(
            format!("soak{}", self.idx),
            self.knobs.timeouts,
            self.mtu_probing,
            self.knobs.rate_control_off,
            self.knobs.start_bytes_per_second,
            self.knobs.first_flight,
        );
        config.resume = ticket;
        config.resumption = credential;
        if let Some(ms) = connect_timeout_ms {
            config.connect_timeout_ms = ms;
        }
        if let Some(sessions) = self.replay.as_mut() {
            // The nonce draw keeps the client's random stream where the captured run had it.
            let _ = self.rng.next_u64();
            let made = sessions
                .pop_front()
                .ok_or(CaptureError::Empty)
                .and_then(ReplayFeed::new)
                .and_then(|feed| NetClient::<SoakGame, _>::replay(socket, config, feed));
            return match made {
                Ok(mut c) => {
                    c.set_capture(self.capture.clone());
                    self.client = Some(c);
                    self.rec.join_attempts.push((now, None));
                    true
                }
                Err(_) => false,
            };
        }
        match NetClient::<SoakGame, _>::connect(socket, server_addr(), config, &(self.idx as u32)) {
            Ok(c) => {
                let nonce = self.rng.next_u64();
                let mut c = c.with_request_nonce(nonce);
                c.set_capture(self.capture.clone());
                self.client = Some(c);
                self.rec.join_attempts.push((now, None));
                true
            }
            Err(_) => false,
        }
    }

    /// One step of the reconnect timeline (ADR-8352), the way `NetLink` drives it: an attempt is
    /// a fresh handshake on a fresh socket that presents the resume ticket and replaces the one in
    /// flight; at the window's end the client gives up and, like a player back at the menu, joins
    /// afresh.
    fn drive_reconnect(&mut self, now: u64) {
        let local = self.local(now);
        let Some(timeline) = self.reconnecting.as_mut() else {
            return;
        };
        let step = timeline.poll(local);
        let timeout = timeline.attempt_timeout_ms(local);
        match step {
            ReconnectStep::Wait => {}
            ReconnectStep::Attempt(_) => {
                self.rec.reconnect_attempts += 1;
                // The attempt in flight goes first (its socket with it); no socket this time
                // leaves the next attempt to try again.
                self.client = None;
                self.open(now, self.ticket, None, Some(timeout));
            }
            ReconnectStep::GiveUp => {
                self.rec.reconnects_given_up += 1;
                self.reconnecting = None;
                self.client = None;
                self.ticket = None;
                self.phase = Phase::Waiting(now);
            }
        }
    }

    fn link_up_after(&self, now: u64) -> u64 {
        self.link.outage_end(now).unwrap_or(now)
    }

    fn close_interval(&mut self, now: u64) {
        if let Some(last) = self.rec.connected.last_mut()
            && last.2 == u64::MAX
        {
            last.2 = now;
        }
    }

    fn make_input(&mut self, tick: u64, now: u64) -> SoakInput {
        if tick.is_multiple_of(20) {
            let r = self.rng.next_u64();
            self.mv = (((r % 3) as i8) - 1, (((r >> 8) % 3) as i8) - 1);
        }
        let heavy = self.heavy.is_some_and(|(a, b)| now >= a && now < b);
        let roll = self.rng.unit();
        let len = if heavy {
            548 + self.rng.up_to((MAX_COMMAND_SAMPLE - 548) as u64) as usize
        } else if roll < 0.03 {
            512 + self.rng.up_to(512) as usize
        } else if roll < 0.18 {
            8 + self.rng.up_to(56) as usize
        } else {
            0
        };
        SoakInput {
            dx: self.mv.0,
            dy: self.mv.1,
            buttons: (tick & 0xff) as u8,
            command: vec![(tick & 0xff) as u8; len.min(MAX_COMMAND_SAMPLE)],
        }
    }

    fn frame(&mut self, now: u64, run: &mut RunState) {
        match self.phase {
            Phase::Waiting(at) | Phase::Left(at) if now >= at => self.connect(now),
            _ => {}
        }
        let Some(mut c) = self.client.take() else {
            // Between two reconnect attempts (the last one could not open a socket).
            self.drive_reconnect(now);
            return;
        };
        let local = self.local(now);
        c.receive(local);
        self.net.take_read_traces(self.ep, &mut self.frame_traces);
        let newest_before = self.newest_snapshot_tick;
        while let Some(ev) = c.poll_event() {
            match ev {
                // A version refusal is not part of the soak record (ADR-7985); interruptions are
                // recorded below for the `interrupted_once` bar. Grade changes (ADR-8889) are not part
                // of it either.
                ClientEvent::VersionMismatch { .. } | ClientEvent::QualityChanged { .. } => {}
                ClientEvent::Connected { peer, .. } => {
                    self.rec.joined = true;
                    self.phase = Phase::Connected;
                    self.rejoin_ticket = None;
                    self.rejoin_credential = None;
                    self.ticket = c.resume_ticket();
                    // The first accept of the reconnect timeline ends the outage, as `NetLink`
                    // reports it (`PeerRestored` after the one `PeerInterrupted`, ADR-8352).
                    if self.reconnecting.take().is_some() {
                        self.rec.interruptions.push((now, "restored".into()));
                    }
                    self.rec.connected.push((u64::from(peer.0), now, u64::MAX));
                    if let Some(a) = self.rec.join_attempts.last_mut()
                        && a.1.is_none()
                    {
                        a.1 = Some(now);
                    }
                    if let Some(r) = self.open_reconnect.take() {
                        run.reconnects[r].connected_ms = Some(now);
                    }
                    self.last_input_tick = None;
                    self.predicted.clear();
                    self.newest_snapshot_tick = None;
                    self.last_snapshot_ms = now;
                    self.send_fail_since = None;
                    self.next_command_ms = now + 500 + self.rng.up_to(1_500);
                }
                ClientEvent::Ended(end) if matches!(self.phase, Phase::Reconnecting) => {
                    // A reconnect attempt's own end. One that timed out or found the server full
                    // leaves room for the next; a refusal for good ends the timeline, and the
                    // client joins afresh (ADR-8352).
                    if end.as_str() == "connect_timeout" {
                        self.rec.connect_timeouts += 1;
                    } else {
                        *self
                            .rec
                            .connect_refusals
                            .entry(end.as_str().to_string())
                            .or_insert(0) += 1;
                    }
                    if !matches!(end.as_str(), "connect_timeout" | "full" | "timeout") {
                        self.reconnecting = None;
                        self.ticket = None;
                        self.phase = Phase::Waiting(now);
                    }
                }
                ClientEvent::Ended(end) => {
                    let voluntary = matches!(self.phase, Phase::Left(_));
                    let peer = c.peer_id().map_or(0, |p| u64::from(p.0));
                    let peer = if peer == 0 {
                        self.rec
                            .connected
                            .last()
                            .filter(|i| i.2 == u64::MAX)
                            .map_or(0, |i| i.0)
                    } else {
                        peer
                    };
                    self.close_interval(now);
                    let was_connected = matches!(self.phase, Phase::Connected);
                    if end.as_str() == "connect_timeout" {
                        self.rec.connect_timeouts += 1;
                    } else if !was_connected && !voluntary {
                        // A join or rejoin attempt refused (`full`, ...): not a lost connection; the
                        // rejoin bar judges how long getting back in took.
                        *self
                            .rec
                            .connect_refusals
                            .entry(end.as_str().to_string())
                            .or_insert(0) += 1;
                    } else {
                        run.ends.push(EndRecord {
                            client: self.idx,
                            at_ms: now,
                            side: "client".into(),
                            cause: end.as_str().to_string(),
                            peer,
                            voluntary,
                        });
                    }
                    if voluntary {
                        // Phase stays Left; the rejoin is scheduled.
                        continue;
                    }
                    if was_connected && self.open_reconnect.is_none() {
                        run.reconnects.push(Reconnect {
                            client: self.idx,
                            kind: "involuntary".into(),
                            start_ms: now,
                            link_up_ms: self.link_up_after(now),
                            connected_ms: None,
                        });
                        self.open_reconnect = Some(run.reconnects.len() - 1);
                    }
                    // A timeout with a resume ticket does not end the game's connection: the
                    // reconnect timeline starts, as in `NetLink` (ADR-8352). The server holds the
                    // peer dormant for its grace, so the first accept is the same `PeerId`.
                    if was_connected
                        && end.as_str() == "timeout"
                        && self.knobs.reconnect.enabled
                        && let Some(ticket) = self.ticket
                    {
                        self.reconnecting = Some(Reconnector::start(
                            self.knobs.reconnect,
                            local,
                            regia_net::reconnect::seed_from(&ticket),
                        ));
                        self.phase = Phase::Reconnecting;
                        continue;
                    }
                    // Retry right away; `connect` takes a new port.
                    self.ticket = None;
                    self.phase = Phase::Waiting(now);
                }
                ClientEvent::Message { message, .. } => self.on_message(message, now),
                // The link's interruption events (ADR-7950), for the `interrupted_once` bar.
                ClientEvent::PeerInterrupted => self.rec.interruptions.push((now, "interrupted".into())),
                ClientEvent::PeerRestored => self.rec.interruptions.push((now, "restored".into())),
                ClientEvent::InputsLate { .. } => {}
            }
        }
        if self.newest_snapshot_tick != newest_before {
            self.carried = self
                .newest_snapshot_tick
                .and_then(|tick| self.carried_by(tick, now, run));
        }
        if matches!(self.phase, Phase::Connected) && c.is_connected() {
            self.connected_frame(&mut c, now, run);
        }
        c.flush(local);
        self.fold_digest(now);
        if let Some(tap) = &self.capture {
            tap.frame(now, self.digest);
        }
        // An ended client is dropped here (its socket handle goes with it).
        if !matches!(c.state(), ClientState::Ended(_)) {
            self.client = Some(c);
        }
        self.drive_reconnect(now);
    }

    /// Traces the snapshot of `tick`, first handed over in this frame, to the datagram that
    /// carried it: of the datagrams this frame read, the first one sent at or after the tick was
    /// made (an unreliable message leaves within a tick of its send or not at all). `None` when no
    /// read datagram can have carried it.
    fn carried_by(&self, tick: u64, now: u64, run: &RunState) -> Option<Carried> {
        let made_us = run.snapshot_made_ms(tick)? * 1000;
        let trace = self
            .frame_traces
            .iter()
            .filter(|t| t.sent_us >= made_us)
            .min_by_key(|t| t.sent_us)?;
        Some(Carried {
            tick,
            made_us,
            trace: *trace,
            handed_us: now * 1000,
        })
    }

    /// A scripted rejoin opens a reconnect record (the run loop calls it before each frame).
    fn before_frame(&mut self, now: u64, run: &mut RunState) {
        if let Phase::Left(at) = self.phase
            && now >= at
            && self.open_reconnect.is_none()
        {
            run.reconnects.push(Reconnect {
                client: self.idx,
                kind: "scripted".into(),
                start_ms: now,
                link_up_ms: self.link_up_after(now),
                connected_ms: None,
            });
            self.open_reconnect = Some(run.reconnects.len() - 1);
        }
    }

    fn on_message(&mut self, message: SoakMsg, now: u64) {
        if self.capture.is_some() {
            // The soak client shows what it is handed: every message goes into its output digest.
            let bytes = regia_net::NetCodec::to_bytes(&message).unwrap_or_default();
            self.seen = Fnv64::new(self.seen).bytes(&bytes).finish();
        }
        match message {
            SoakMsg::Snapshot {
                tick, own_x, own_y, ..
            } => {
                self.rec.snapshot_parts += 1;
                self.last_snapshot_ms = now;
                if self.newest_snapshot_tick.is_some_and(|t| t >= tick) {
                    return;
                }
                self.newest_snapshot_tick = Some(tick);
                self.newest_snapshot_seen_ms = now;
                // Compare with the prediction at that tick.
                while self.predicted.front().is_some_and(|&(t, _, _)| t < tick) {
                    self.predicted.pop_front();
                }
                if let Some(&(t, x, y)) = self.predicted.front()
                    && t == tick
                {
                    self.rec.corrections_checked += 1;
                    if (x, y) != (own_x, own_y) {
                        self.rec.corrections += 1;
                        let steps = ((x - own_x).unsigned_abs() + (y - own_y).unsigned_abs())
                            / STEP_UNITS.unsigned_abs();
                        self.rec.correction_steps.add(u64::from(steps));
                        let head = self.predicted.back().map_or(t, |p| p.0);
                        self.rec.rollback_ticks.add(head - t);
                        // Motion is additive: replaying the inputs from the server's state is
                        // a shift of every later prediction.
                        let (sx, sy) = (own_x - x, own_y - y);
                        for p in &mut self.predicted {
                            p.1 += sx;
                            p.2 += sy;
                        }
                    }
                }
            }
            SoakMsg::CommandAck { id } => {
                if let Some((sent, i)) = self.pending_commands.remove(&id) {
                    self.rec.commands_acked += 1;
                    self.rec.command_rtt_frames.add((now - sent) / FRAME_MS);
                    if let Some(e) = self.rec.command_log.get_mut(i) {
                        e.1 = Some(now);
                    }
                }
            }
            SoakMsg::Bulk { of, .. } => {
                self.rec.bulk_chunks += 1;
                if self.rec.bulk_chunks >= of && self.rec.bulk_done_ms.is_none() {
                    self.rec.bulk_done_ms = Some(now);
                }
            }
            SoakMsg::World { .. } => {
                let connected_at = self.rec.connected.last().map_or(now, |i| i.1);
                self.rec
                    .world_after_connect_ms
                    .push(now.saturating_sub(connected_at));
                // The join snapshot of the current connection (one per connection).
                if let Some(&(_, from, _)) = self.rec.connected.last()
                    && self.rec.join_snapshots.last().is_none_or(|&(c, _)| c != from)
                {
                    self.rec.join_snapshots.push((from, now));
                }
            }
            SoakMsg::Chat { .. } | SoakMsg::Command { .. } => {}
        }
    }

    fn connected_frame(&mut self, c: &mut NetClient<SoakGame, ImpairedSocket>, now: u64, run: &RunState) {
        // Scripted leave.
        if self.leave_at.is_some_and(|t| now >= t) {
            self.leave_at = None;
            self.phase = Phase::Left(now + 2_000);
            self.rejoin_ticket = c.resume_ticket();
            self.rejoin_credential = c.resumption();
            self.ticket = None;
            self.rec.leaves.push(now);
            c.disconnect();
            self.close_interval(now);
            return;
        }
        let est = c.estimated_server_tick(self.local(now));
        if let Some(t) = self.newest_snapshot_tick {
            let stale = est.saturating_sub(t);
            self.rec.staleness_ticks.add(stale);
            if !self.link.in_outage(now) {
                self.rec.staleness_ticks_link_up.add(stale);
                self.split_staleness(stale, t, now, run);
                match self.carried.filter(|c| c.tick == t) {
                    Some(c) => self.rec.staleness_stages.add(stale, c.stages(stale, now * 1000)),
                    None => self.rec.staleness_stages.untraced += 1,
                }
                let connected_at = self.rec.connected.last().map_or(now, |i| i.1);
                if now >= connected_at + STALENESS_SETTLE_MS {
                    self.rec.staleness_ticks_settled.add(stale);
                }
            }
            let second = (now / 1000) as usize;
            if self.rec.staleness_max_per_s.len() <= second {
                self.rec.staleness_max_per_s.resize(second + 1, 0);
            }
            let slot = &mut self.rec.staleness_max_per_s[second];
            *slot = (*slot).max(stale.min(u64::from(u16::MAX)) as u16);
        }
        let ticks = match self.knobs.input_lead {
            // The engine's loop (ADR-8077): every tick once, up to the estimate plus the lead the
            // server's input feedback steers.
            InputLead::Steered => c.due_input_ticks(self.local(now)),
            // Input feedback off: a fixed lead and at most three ticks of catch-up.
            InputLead::Fixed(lead) => {
                let target = est + lead;
                let from = self
                    .last_input_tick
                    .map_or(target, |t| t + 1)
                    .max(target.saturating_sub(3));
                from..target + 1
            }
        };
        let silent = self.knobs.silent_inputs.is_some_and(|q| q.covers(self.idx, now));
        for tick in ticks {
            if silent {
                // The game sends nothing for this tick (ADR-8887); the loop moves on as if it had.
                self.last_input_tick = Some(tick);
                continue;
            }
            let input = self.make_input(tick, now);
            if tick < est {
                self.rec.inputs_stamped_behind += 1;
            }
            // A skipped tick moves nothing on the server either, so continue from the last one.
            let (px, py) = self.predicted.back().map_or((0, 0), |&(_, x, y)| (x, y));
            let (nx, ny) = (
                px + i32::from(input.dx) * STEP_UNITS,
                py + i32::from(input.dy) * STEP_UNITS,
            );
            match c.send_input(tick, input) {
                Ok(()) => {
                    self.rec.inputs_sent += 1;
                    if let Some(since) = self.send_fail_since.take() {
                        self.note_send_gap(since, now);
                    }
                }
                Err(e) => {
                    *self
                        .rec
                        .input_send_errors
                        .entry(send_error_kind(&e).into())
                        .or_insert(0) += 1;
                    *self
                        .send_fail_kinds
                        .entry(send_error_kind(&e).into())
                        .or_insert(0) += 1;
                    self.send_fail_since.get_or_insert(now);
                }
            }
            // The game predicts whether or not the transport took the input: the player pressed it.
            self.predicted.push_back((tick, nx, ny));
            while self.predicted.len() > 256 {
                self.predicted.pop_front();
            }
            self.last_input_tick = Some(tick);
        }
        // Snapshot gap, judged from the end of the excuse of any outage it overlaps (ADR-8887).
        let from = self.judged_from(self.last_snapshot_ms, now);
        if now.saturating_sub(from) >= WEDGE_MS {
            self.pending_gap(from, now);
        }
        // Reliable traffic: a 0.5-4 KiB command every 2-4 s, a chat line every 3 s.
        if now >= self.next_command_ms {
            self.next_command_ms = now + 2_000 + self.rng.up_to(2_000);
            let len = 512 + self.rng.up_to(4096 - 512) as usize;
            let id = self.next_command_id;
            self.next_command_id += 1;
            let msg = SoakMsg::Command {
                id,
                payload: vec![(id & 0xff) as u8; len],
            };
            match c.send(Channel::GAME_RPC, &msg) {
                Ok(()) => {
                    self.rec.commands_sent += 1;
                    self.rec.command_log.push((now, None));
                    self.pending_commands
                        .insert(id, (now, self.rec.command_log.len() - 1));
                }
                Err(e) => {
                    *self
                        .rec
                        .reliable_send_errors
                        .entry(send_error_kind(&e).into())
                        .or_insert(0) += 1;
                }
            }
            if id.is_multiple_of(2) {
                self.chat_seq += 1;
                let chat = SoakMsg::Chat {
                    seq: self.chat_seq,
                    text: b"gg".to_vec(),
                };
                if let Err(e) = c.send(CHAT_CHANNEL, &chat) {
                    *self
                        .rec
                        .reliable_send_errors
                        .entry(send_error_kind(&e).into())
                        .or_insert(0) += 1;
                }
            }
        }
    }

    /// Where a client-side gap that started at `since` is judged from at `now`: after every outage
    /// it overlaps, plus [`INPUT_GRACE_MS`] (the link needs a moment back), or from `since` when it
    /// overlaps none (a gap that starts after an outage has ended is judged whole, as before).
    /// Before ADR-8887 any overlapping outage excused the whole gap, so a client that heard nothing
    /// after an outage was never wedged.
    fn judged_from(&self, since: u64, now: u64) -> u64 {
        self.link
            .outages
            .iter()
            .filter(|&&(s, e)| s < now && e > since)
            .map(|&(_, e)| e.saturating_add(INPUT_GRACE_MS))
            .fold(since, u64::max)
    }

    /// Splits one link-up staleness sample into its parts (ADR-10096, [`StalenessParts`]):
    /// `stale` is the bar's sample, `t` the newest snapshot's tick.
    fn split_staleness(&mut self, stale: u64, t: u64, now: u64, run: &RunState) {
        // The latency no sender can beat: the server's uplink plus this client's downlink.
        let path_ms =
            run.server_link.directions_at(now).0.latency_ms + self.link.directions_at(now).1.latency_ms;
        let path_ticks = path_ms * TICK_HZ / 1000;
        self.rec
            .staleness_beyond_path_link_up
            .add(stale.saturating_sub(path_ticks));
        // The server sent the snapshot of tick `t` in the iteration that ran the tick; a snapshot
        // it has no send time for (never the case for one a client holds) counts as sent now.
        let sent_ms = run.snapshot_sent_ms.get(t as usize).copied().unwrap_or(now);
        let seen_ms = self.newest_snapshot_seen_ms.clamp(sent_ms, now);
        let k = MILLITICKS_PER_MS;
        let (path, age) = (path_ms as i64 * k, (now - sent_ms) as i64 * k);
        let hold = (now - seen_ms) as i64 * k;
        let whole = run.server_tick.saturating_sub(t) as i64 * 1000;
        let p = &mut self.rec.staleness_parts;
        if p.samples == 0 {
            p.path_ticks_min = path_ticks;
            p.path_ticks_max = path_ticks;
        } else {
            p.path_ticks_min = p.path_ticks_min.min(path_ticks);
            p.path_ticks_max = p.path_ticks_max.max(path_ticks);
        }
        p.samples += 1;
        p.total += stale as i64 * 1000;
        p.path += path;
        p.transit += age - hold - path;
        p.hold += hold;
        p.rounding += whole - age;
        p.estimate += stale as i64 * 1000 - whole;
    }

    fn note_send_gap(&mut self, since: u64, now: u64) {
        let from = self.judged_from(since, now);
        if now.saturating_sub(from) >= WEDGE_MS {
            CLIENT_GAPS.with(|g| {
                g.borrow_mut().push(Wedge {
                    client: self.idx,
                    kind: "input_send_failing".into(),
                    start_ms: from,
                    length_ms: now - from,
                    send_errors: std::mem::take(&mut self.send_fail_kinds),
                })
            });
        }
        self.send_fail_kinds.clear();
    }

    fn pending_gap(&mut self, from: u64, now: u64) {
        CLIENT_GAPS.with(|g| {
            g.borrow_mut().push(Wedge {
                client: self.idx,
                kind: "no_snapshot".into(),
                start_ms: from,
                length_ms: now - from,
                send_errors: BTreeMap::new(),
            })
        });
        // One record per gap: restart the clock.
        self.last_snapshot_ms = now;
    }

    fn finish(&mut self, now: u64) {
        if let Some(since) = self.send_fail_since.take() {
            self.note_send_gap(since, now);
        }
        self.close_interval(now);
    }
}

thread_local! {
    static CLIENT_GAPS: std::cell::RefCell<Vec<Wedge>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct RunState {
    ends: Vec<EndRecord>,
    reconnects: Vec<Reconnect>,
    /// The server's link (its uplink is part of every client's path).
    server_link: LinkSpec,
    /// The tick the server is on: the one it runs next.
    server_tick: u64,
    /// When the server sent the snapshot of each tick (index = tick), virtual milliseconds.
    snapshot_sent_ms: Vec<u64>,
    /// When the server made the snapshot of each tick, milliseconds, from `first_tick` on
    /// (ADR-10145).
    snapshot_ms: Vec<u64>,
    first_tick: Option<u64>,
}

impl RunState {
    fn new(server_link: LinkSpec) -> Self {
        Self {
            ends: Vec::new(),
            reconnects: Vec::new(),
            server_link,
            server_tick: 0,
            snapshot_sent_ms: Vec::new(),
            snapshot_ms: Vec::new(),
            first_tick: None,
        }
    }

    /// The server made the snapshot of `tick` at `now_ms` (ticks come in order, one at a time).
    fn note_snapshot(&mut self, tick: u64, now_ms: u64) {
        let first = *self.first_tick.get_or_insert(tick);
        // A tick the loop skipped (none does) takes the time of the next one made.
        while first + (self.snapshot_ms.len() as u64) <= tick && self.snapshot_ms.len() < (1 << 24) {
            self.snapshot_ms.push(now_ms);
        }
    }

    /// When the server made the snapshot of `tick`, milliseconds.
    fn snapshot_made_ms(&self, tick: u64) -> Option<u64> {
        let i = tick.checked_sub(self.first_tick?)?;
        self.snapshot_ms.get(usize::try_from(i).ok()?).copied()
    }
}

struct PeerState {
    client: Option<usize>,
    x: i32,
    y: i32,
    first_input: bool,
    last_input_ms: u64,
    joined_ms: u64,
    /// Timed out and held for its resume ticket (`ServerEvent::PeerDormant`): the avatar stays in
    /// the world, nothing is sent to the peer and none of its ticks are judged until it resumes.
    dormant: bool,
}

impl PeerState {
    /// The client's open input gap at `now_ms`, `(start, end)`, when it is at least [`WEDGE_MS`]
    /// long: from its last input, or, before the first one ever arrived, from the input grace
    /// after its join (the moment `missed_input` starts judging it, ADR-8424). A peer that is
    /// removed, or still there at the end of the run, closes its gap this way (ADR-8887).
    fn open_gap(&self, now_ms: u64) -> Option<(u64, u64)> {
        let since = if self.first_input {
            self.last_input_ms
        } else {
            self.joined_ms + INPUT_GRACE_MS
        };
        (now_ms.saturating_sub(since) >= WEDGE_MS).then_some((since, now_ms))
    }
}

struct Server {
    server: NetServer<SoakGame, ImpairedSocket>,
    peers: BTreeMap<PeerId, PeerState>,
    npcs: Vec<EntityState>,
    rec: ServerRecord,
    net: ImpairedNet,
    client_eps: Vec<EndpointId>,
    /// The server's last known address of every client peer.
    addrs: BTreeMap<PeerId, SocketAddr>,
    /// `(start ms, bytes)` of the bulk transfer.
    bulk: Option<(u64, usize)>,
    /// Peers still owed bulk chunks: next chunk index.
    bulk_next: BTreeMap<PeerId, u32>,
    /// [`Knobs::silent_snapshots`].
    silent_snapshots: Option<Silence>,
}

const HOSTILE_JOIN: u32 = u32::MAX;

impl Server {
    fn new(
        net: &ImpairedNet,
        ep: EndpointId,
        s: &Scenario,
        client_eps: Vec<EndpointId>,
        knobs: &Knobs,
    ) -> Self {
        let (timeouts, rate_control_off) = (knobs.timeouts, knobs.rate_control_off);
        let config = ServerConfig {
            max_peers: s.clients + 8,
            tick_rate_hz: TICK_HZ as u32,
            health: HealthPolicy::default().with_timeouts(timeouts),
            link: soak_link_config(
                s.profile.mtu_probing,
                rate_control_off,
                knobs.start_bytes_per_second,
                knobs.first_flight,
            ),
            // A listen server shares the host's uplink (spec 120 § 4.2 item 8).
            shared_uplink: s.mode == ServerMode::Listen,
            resume_grace_ms: knobs
                .server_resume_grace_ms
                .unwrap_or_else(|| s.profile.server_resume_grace_ms()),
            ..ServerConfig::default()
        };
        let mut rng = SoakRng::derive(s.seed, 7);
        let words = [rng.next_u64(), rng.next_u64(), rng.next_u64(), rng.next_u64()];
        let mut secret = [0u8; 32];
        for (i, b) in secret.iter_mut().enumerate() {
            *b = (words[i / 8] >> ((i % 8) * 8)) as u8;
        }
        let server = NetServer::with_secret(net.socket(ep), config, secret);
        let npc_count = TARGET_ENTITIES.saturating_sub(s.clients).max(64);
        let npcs: Vec<EntityState> = (0..npc_count)
            .map(|i| EntityState {
                id: 1_000 + i as u16,
                x: (i as i32) * 64,
                y: 0,
                yaw: 0,
            })
            .collect();
        let world_entities = s.clients + npcs.len();
        Self {
            server,
            peers: BTreeMap::new(),
            npcs,
            rec: ServerRecord {
                world_entities,
                ..ServerRecord::default()
            },
            net: net.clone(),
            client_eps,
            addrs: BTreeMap::new(),
            bulk: s.profile.bulk,
            bulk_next: BTreeMap::new(),
            silent_snapshots: knobs.silent_snapshots,
        }
    }

    /// Records every change of the server's address for a client's peer, judged against the
    /// addresses that client really held.
    fn check_addresses(&mut self, now: u64) {
        for (peer, p) in &self.peers {
            let Some(client) = p.client else { continue };
            let Some(addr) = self.server.session().peer(*peer).map(|x| x.addr) else {
                continue;
            };
            match self.addrs.insert(*peer, addr) {
                Some(old) if old != addr => {
                    let held = self
                        .client_eps
                        .get(client)
                        .map(|&ep| self.net.addresses_held(ep))
                        .unwrap_or_default();
                    self.rec.address_moves.push(AddressMove {
                        client,
                        peer: u64::from(peer.0),
                        at_ms: now,
                        foreign: !held.contains(&addr),
                    });
                }
                _ => {}
            }
        }
    }

    /// Sends the join's world blob to `peer` (a join, or a resume: the full state first).
    fn send_world(&mut self, peer: PeerId) {
        let blob = SoakMsg::World {
            tick: self.server.driver().tick(),
            blob: vec![0x5A; WORLD_BLOB_BYTES],
        };
        if let Err(e) = self.server.send(peer, JOIN_CHANNEL, &blob) {
            *self
                .rec
                .snapshot_send_errors
                .entry(format!("world_{}", send_error_kind(&e)))
                .or_insert(0) += 1;
        }
    }

    /// Starts the bulk transfer when it is due and sends what the transport takes.
    fn pump_bulk(&mut self, now: u64) {
        let Some((start, bytes)) = self.bulk else { return };
        let chunks = bytes.div_ceil(BULK_CHUNK).max(1) as u32;
        if now >= start && self.rec.bulk_start_ms.is_none() {
            self.rec.bulk_start_ms = Some(now);
            for (peer, p) in &self.peers {
                if p.client.is_some() {
                    self.bulk_next.insert(*peer, 0);
                }
            }
        }
        let peers: Vec<PeerId> = self.bulk_next.keys().copied().collect();
        for peer in peers {
            let Some(mut next) = self.bulk_next.get(&peer).copied() else {
                continue;
            };
            while next < chunks {
                let msg = SoakMsg::Bulk {
                    seq: next,
                    of: chunks,
                    data: vec![0xB0; BULK_CHUNK.min(bytes - next as usize * BULK_CHUNK)],
                };
                // The bulk stream (spec 120 § 5.1): it yields to gameplay (ADR-8302).
                match self.server.send(peer, BULK_CHANNEL, &msg) {
                    Ok(()) => next += 1,
                    Err(SendError::Backpressure(_)) => break,
                    Err(e) => {
                        *self
                            .rec
                            .snapshot_send_errors
                            .entry(format!("bulk_{}", send_error_kind(&e)))
                            .or_insert(0) += 1;
                        next = chunks;
                    }
                }
            }
            if next >= chunks {
                self.bulk_next.remove(&peer);
            } else {
                self.bulk_next.insert(peer, next);
            }
        }
    }

    fn iteration(&mut self, now: u64, links: &[LinkSpec], run: &mut RunState) {
        let started = Instant::now();
        let allocs_before = super::probe::allocs().map(|a| (a, super::probe::harness_allocs()));
        let us = |t: Instant| t.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
        let mut phases = [0u32; 5];
        let mut mark = Instant::now();
        self.server.receive(now);
        phases[0] = us(mark);
        mark = Instant::now();
        while let Some(ev) = self.server.poll_event() {
            match ev {
                ServerEvent::PeerJoined { peer, join, .. } => {
                    let client = (join != HOSTILE_JOIN).then_some(join as usize);
                    self.peers.insert(
                        peer,
                        PeerState {
                            client,
                            x: 0,
                            y: 0,
                            first_input: false,
                            last_input_ms: now,
                            joined_ms: now,
                            dormant: false,
                        },
                    );
                    self.send_world(peer);
                }
                // A timed-out player held for its ticket (spec 120 § 10.3): its connection ended
                // here, by the same policy and at the same time as a removal, so the record gets
                // the same end; the avatar waits.
                ServerEvent::PeerDormant { peer, reason } => {
                    self.addrs.remove(&peer);
                    self.bulk_next.remove(&peer);
                    if let Some(p) = self.peers.get_mut(&peer) {
                        p.dormant = true;
                        if let Some(client) = p.client {
                            run.ends.push(EndRecord {
                                client,
                                at_ms: now,
                                side: "server".into(),
                                cause: reason.as_str().to_string(),
                                peer: u64::from(peer.0),
                                voluntary: false,
                            });
                        }
                    }
                }
                // The same peer on a new connection, from dormancy or over a connection the
                // server still held: the avatar carries on where it stood, and the connection
                // starts like a join's (the world blob again, the input grace again).
                ServerEvent::PeerResumed { peer, join, .. } => {
                    let client = (join != HOSTILE_JOIN).then_some(join as usize);
                    let p = self.peers.entry(peer).or_insert(PeerState {
                        client,
                        x: 0,
                        y: 0,
                        first_input: false,
                        last_input_ms: now,
                        joined_ms: now,
                        dormant: false,
                    });
                    p.dormant = false;
                    p.first_input = false;
                    p.last_input_ms = now;
                    p.joined_ms = now;
                    self.send_world(peer);
                }
                ServerEvent::PeerLeft { peer, reason } => {
                    self.addrs.remove(&peer);
                    self.bulk_next.remove(&peer);
                    if let Some(p) = self.peers.remove(&peer)
                        // A dormant peer's end was recorded when its connection ended.
                        && !p.dormant
                        && let Some(client) = p.client
                    {
                        // The gap the peer was in when it went: before ADR-8887 a peer removed
                        // while its input was missing was never judged (the gap closed only on
                        // the next input or at the end of the run).
                        if let Some((s, e)) = p.open_gap(now) {
                            self.rec.input_gaps.push((client, u64::from(peer.0), s, e));
                        }
                        run.ends.push(EndRecord {
                            client,
                            at_ms: now,
                            side: "server".into(),
                            cause: reason.as_str().to_string(),
                            peer: u64::from(peer.0),
                            voluntary: reason.as_str() == "left",
                        });
                    }
                }
                ServerEvent::Message { peer, message, .. } => {
                    if let SoakMsg::Command { id, .. } = message {
                        let _ = self
                            .server
                            .send(peer, Channel::GAME_RPC, &SoakMsg::CommandAck { id });
                    }
                }
                // Events the transport gained after this kit was written (ADR-7892, ADR-7950):
                // the records read their effects (ends, wedges), not the events.
                ServerEvent::PeerInterrupted { .. }
                | ServerEvent::PeerRestored { .. }
                | ServerEvent::Rejected { .. }
                | ServerEvent::QualityChanged { .. } => {}
            }
        }
        phases[1] = us(mark);
        mark = Instant::now();
        self.check_addresses(now);
        self.pump_bulk(now);
        phases[2] = us(mark);
        mark = Instant::now();
        let mut ticked = false;
        while self.server.driver().tick() * 1000 / TICK_HZ <= now {
            ticked = true;
            self.tick(now, links);
            // The snapshot of the tick just run leaves in this iteration's flush.
            let next = self.server.driver().tick();
            run.server_tick = next;
            let sent = next.saturating_sub(1) as usize;
            if run.snapshot_sent_ms.len() <= sent {
                run.snapshot_sent_ms.resize(sent + 1, now);
            }
            run.note_snapshot(self.server.driver().tick().saturating_sub(1), now);
        }
        phases[3] = us(mark);
        mark = Instant::now();
        self.server.flush(now);
        phases[4] = us(mark);
        let total = us(started);
        self.rec.busy_us += u64::from(total);
        if ticked {
            self.rec.tick_us.push(total);
            self.rec.tick_phases_us.push(phases);
            if let (Some((a0, h0)), Some(a1)) = (allocs_before, super::probe::allocs()) {
                let engine = a1
                    .saturating_sub(a0)
                    .saturating_sub(super::probe::harness_allocs() - h0);
                self.rec.tick_allocs.push(engine.min(u64::from(u32::MAX)) as u32);
            }
        }
    }

    fn tick(&mut self, now: u64, links: &[LinkSpec]) {
        let peers = &mut self.peers;
        let rec = &mut self.rec;
        let inputs = self.server.advance_tick(|_, _| {});
        rec.ticks += 1;
        for (peer, p) in peers.iter_mut().filter(|(_, p)| !p.dormant) {
            let has = inputs.contains_key(peer);
            if let Some(c) = p.client
                && !links[c].in_outage(now)
                && (p.first_input || has || now >= p.joined_ms + INPUT_GRACE_MS)
            {
                *rec.input_ticks_judged.entry(c).or_insert(0) += 1;
                if has {
                    *rec.input_ticks_arrived.entry(c).or_insert(0) += 1;
                }
            }
            match inputs.get(peer) {
                Some(input) => {
                    p.x += i32::from(input.dx) * STEP_UNITS;
                    p.y += i32::from(input.dy) * STEP_UNITS;
                    if let Some(c) = p.client {
                        if p.first_input {
                            *rec.ticks_with_input.entry(c).or_insert(0) += 1;
                        }
                        // Recorded whole: `report::wedges` excuses the outages it overlaps and
                        // judges the rest (ADR-8887). Closed like a removal or the run's end, so
                        // a never-heard peer's gap starts at its join grace, not at the join
                        // (ADR-8424, ADR-10107).
                        if let Some((s, e)) = p.open_gap(now) {
                            rec.input_gaps.push((c, u64::from(peer.0), s, e));
                        }
                    }
                    p.first_input = true;
                    p.last_input_ms = now;
                }
                None => {
                    if let Some(c) = p.client
                        && p.first_input
                        && !links[c].in_outage(now)
                    {
                        *rec.ticks_missed.entry(c).or_insert(0) += 1;
                    }
                }
            }
        }
        let tick = self.server.driver().tick().saturating_sub(1);
        // Every avatar plus the NPCs, avatars first.
        let mut world: Vec<EntityState> = self
            .peers
            .iter()
            .filter(|(_, p)| p.client.is_some())
            .map(|(id, p)| EntityState {
                id: id.0 as u16,
                x: p.x,
                y: p.y,
                yaw: 0,
            })
            .collect();
        for npc in &mut self.npcs {
            npc.yaw = npc.yaw.wrapping_add(1);
            npc.y = (tick % 600) as i32;
        }
        world.extend_from_slice(&self.npcs);
        let ids: Vec<PeerId> = self.peers.keys().copied().collect();
        for id in ids {
            let Some(p) = self.peers.get_mut(&id).filter(|p| !p.dormant) else {
                continue;
            };
            let budget = entity_budget(self.server.send_allowance(id), world.len());
            // `random_loss_kept` reads the budget once the peer's path has answered (ADR-10200):
            // during a first flight (a join or a rejoin, ADR-10156) the allowance is the essential
            // floor's rate by design, not what random loss left of it.
            if now > 5_000 && p.client.is_some() {
                let least = if self.server.in_first_flight(id) {
                    &mut self.rec.min_flight_entity_budget
                } else {
                    &mut self.rec.min_entity_budget
                };
                *least = Some(least.map_or(budget, |m| m.min(budget)));
            }
            if let (Some(q), Some(c)) = (self.silent_snapshots, p.client)
                && q.covers(c, now)
            {
                // The knob's withheld snapshots (ADR-8887).
                continue;
            }
            let (own_x, own_y) = (p.x, p.y);
            let chunks: Vec<&[EntityState]> = world[..budget].chunks(PART_ENTITIES).collect();
            let parts = chunks.len().max(1) as u8;
            for (part, chunk) in chunks.iter().enumerate() {
                let msg = SoakMsg::Snapshot {
                    tick,
                    own_x,
                    own_y,
                    part: part as u8,
                    parts,
                    entities: chunk.to_vec(),
                };
                if let Err(e) = self.server.send(id, Channel::Unreliable, &msg) {
                    *self
                        .rec
                        .snapshot_send_errors
                        .entry(send_error_kind(&e).into())
                        .or_insert(0) += 1;
                }
            }
        }
    }

    fn finish(&mut self, now: u64) {
        for (peer, p) in self.peers.iter().filter(|(_, p)| !p.dormant) {
            // A peer none of whose inputs ever reached the server is wedged from the grace after
            // its join (ADR-8424): the client is connected by then on any link the plans run, and
            // `missed_input` judges its ticks from the same moment.
            if let Some(c) = p.client
                && let Some((s, e)) = p.open_gap(now)
            {
                self.rec.input_gaps.push((c, u64::from(peer.0), s, e));
            }
        }
        self.rec.totals = self.server.totals();
    }
}

/// Hostile session windows, relative to the hostile start.
const HOSTILE_START_MS: u64 = 10_000;
const HOSTILE_WINDOW_MS: u64 = 10_000;
const HOSTILE_KINDS: [&str; 3] = ["over_cap_input", "malformed_messages", "flood"];
const FLOOD_KINDS: [&str; 1] = ["sealed_flood"];

struct Hostile {
    kinds: &'static [&'static str],
    raw_attacks: bool,
    net: ImpairedNet,
    conn_ep: EndpointId,
    raw: ImpairedSocket,
    raw_ep: EndpointId,
    /// [`Knobs::hostile_relay`].
    relay: bool,
    victim: EndpointId,
    client: Option<NetClient<HostileGame, ImpairedSocket>>,
    session: Option<usize>,
    tick: u64,
    rng: SoakRng,
    rec: HostileRecord,
}

impl Hostile {
    fn new(
        net: &ImpairedNet,
        s: &Scenario,
        victim: EndpointId,
        flood_only: bool,
        relay: bool,
    ) -> Option<Self> {
        let link = s.profile.client_link(0, s.duration_ms);
        let conn_ep = net
            .add_endpoint(SocketAddr::from(([10, 200, 0, 1], 40_000)), link.clone())
            .ok()?;
        let raw_ep = net
            .add_endpoint(SocketAddr::from(([10, 200, 0, 2], 40_000)), link)
            .ok()?;
        net.tap(victim);
        Some(Self {
            kinds: if flood_only { &FLOOD_KINDS } else { &HOSTILE_KINDS },
            raw_attacks: !flood_only,
            net: net.clone(),
            conn_ep,
            raw: net.socket(raw_ep),
            raw_ep,
            relay: relay && !flood_only,
            victim,
            client: None,
            session: None,
            tick: 0,
            rng: SoakRng::derive(s.seed, 99),
            rec: HostileRecord::default(),
        })
    }

    fn frame(&mut self, now: u64) {
        use regia_net::DatagramSocket;
        if now < HOSTILE_START_MS {
            return;
        }
        let k = ((now - HOSTILE_START_MS) / HOSTILE_WINDOW_MS) as usize;
        // Start a new session at each window.
        if k < self.kinds.len() && self.session.is_none_or(|s| s < k) {
            if let Some(mut old) = self.client.take() {
                old.disconnect();
                old.flush(now);
            }
            self.net.rebind_now(self.conn_ep);
            let start = HOSTILE_START_MS + k as u64 * HOSTILE_WINDOW_MS;
            self.rec.sessions.push(HostileSession {
                kind: self.kinds[k].into(),
                start_ms: start,
                end_ms: start + HOSTILE_WINDOW_MS,
                ..HostileSession::default()
            });
            self.session = Some(k);
            self.client = NetClient::<HostileGame, _>::connect(
                self.net.socket(self.conn_ep),
                server_addr(),
                // A hostile peer does not run the engine's pacer: its sends leave unpaced, whatever
                // the honest clients' rate control (ADR-8530).
                client_config(
                    "hostile".into(),
                    TimeoutPolicy::default(),
                    false,
                    true,
                    None,
                    None,
                ),
                &HOSTILE_JOIN,
            )
            .ok();
        }
        // Raw attacks from the second address, the whole time after the start.
        if self.raw_attacks && now < HOSTILE_START_MS + HOSTILE_WINDOW_MS * self.kinds.len() as u64 {
            if (now / FRAME_MS).is_multiple_of(6) {
                let taps = self.net.tapped(self.victim);
                for d in taps.iter().rev().take(16) {
                    let _ = self.raw.send_to(d, server_addr());
                    self.rec.replays_sent += 1;
                }
            }
            for _ in 0..8 {
                let len = 1 + self.rng.up_to(1_399) as usize;
                let bytes: Vec<u8> = (0..len).map(|_| self.rng.next_u64() as u8).collect();
                let _ = self.raw.send_to(&bytes, server_addr());
                self.rec.garbage_sent += 1;
            }
            if (now / FRAME_MS).is_multiple_of(6) {
                let len = 4_000 + self.rng.up_to(56_000) as usize;
                let _ = self.raw.send_to(&vec![0xEE; len], server_addr());
                self.rec.oversized_sent += 1;
            }
        }
        if self.relay {
            // A full relay: the server's answers to the replay address (its path probes, and all
            // the downstream once the peer moved there) go on to the victim under the server's
            // address, so the victim acknowledges them in the records the next replay copies.
            let mut buf = vec![0u8; super::impair::MAX_WIRE_DATAGRAM];
            let victim = self.net.public_addr(self.victim);
            while let Ok(Some((n, from))) = self.raw.recv_from(&mut buf) {
                if from == server_addr() {
                    self.net.send_forged(self.raw_ep, from, &buf[..n], victim);
                }
            }
        }
        let Some(idx) = self.session else { return };
        let Some(mut c) = self.client.take() else { return };
        c.receive(now);
        while let Some(ev) = c.poll_event() {
            match ev {
                // Interruption, version and input-feedback events are not part of the soak record
                // (merged lanes: ADR-7950, ADR-7985, ADR-8077).
                ClientEvent::PeerInterrupted
                | ClientEvent::PeerRestored
                | ClientEvent::VersionMismatch { .. }
                | ClientEvent::InputsLate { .. }
                | ClientEvent::QualityChanged { .. } => {}
                ClientEvent::Connected { .. } => self.rec.sessions[idx].connected_ms = Some(now),
                ClientEvent::Ended(end) => {
                    let s = &mut self.rec.sessions[idx];
                    if s.dropped_ms.is_none() && end.as_str() != "closed" {
                        s.dropped_ms = Some(now);
                        s.end_cause = Some(end.as_str().to_string());
                    }
                }
                ClientEvent::Message { .. } => {}
            }
        }
        if c.is_connected() && now < self.rec.sessions[idx].end_ms {
            let session = &mut self.rec.sessions[idx];
            let mut acts = 0u64;
            match self.kinds[idx] {
                "over_cap_input" => {
                    // One over-cap sample per tick inside an otherwise valid record.
                    self.tick += 1;
                    if c.send_input(self.tick, over_cap_input(60_000, 8)).is_ok() {
                        acts += 1;
                    }
                }
                "malformed_messages" => {
                    // One undecodable message per frame, and a 60 KiB undecodable reliable message
                    // every second (it is reassembled before it is refused).
                    let len = 1 + self.rng.up_to(64) as usize;
                    let mut bytes = vec![0xF0];
                    bytes.extend((0..len).map(|_| self.rng.next_u64() as u8));
                    if c.send(Channel::Unreliable, &RawBytes(bytes)).is_ok() {
                        acts += 1;
                    }
                    if (now / FRAME_MS).is_multiple_of(60)
                        && c.send(Channel::GAME_RPC, &RawBytes(vec![0xF1; 60_000])).is_ok()
                    {
                        acts += 1;
                    }
                }
                "sealed_flood" => {
                    // One valid sealed datagram per send: a small chat line, flushed at once.
                    let chat = SoakMsg::Chat {
                        seq: 0,
                        text: b"x".to_vec(),
                    };
                    let mut w = regia_net::WireWriter::new();
                    regia_net::NetCodec::encode(&chat, &mut w);
                    let chat = RawBytes(w.finish());
                    for _ in 0..FLOOD_DATAGRAMS_PER_S * FRAME_MS / 1000 {
                        if c.send(Channel::Unreliable, &chat).is_ok() {
                            acts += 1;
                        }
                        c.flush(now);
                    }
                }
                _ => {
                    // Valid chat lines as fast as the transport takes them.
                    let mut w = regia_net::WireWriter::new();
                    regia_net::NetCodec::encode(
                        &SoakMsg::Chat {
                            seq: 0,
                            text: b"spam".to_vec(),
                        },
                        &mut w,
                    );
                    let chat = RawBytes(w.finish());
                    for _ in 0..3_000 {
                        if c.send(Channel::Unreliable, &chat).is_ok() {
                            acts += 1;
                        }
                    }
                    for _ in 0..1_100 {
                        if c.send(CHAT_CHANNEL, &chat).is_err() {
                            break;
                        }
                        acts += 1;
                    }
                }
            }
            if acts > 0 {
                session.acts += acts;
                session.first_act_ms.get_or_insert(now);
            }
        }
        c.flush(now);
        if !matches!(c.state(), ClientState::Ended(_)) {
            self.client = Some(c);
        }
    }
}

/// A socket that only keeps what is sent through it (to capture real `ConnectRequest` bytes).
#[derive(Debug, Clone, Default)]
struct CaptureSocket {
    sent: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

impl regia_net::DatagramSocket for CaptureSocket {
    fn send_to(&mut self, bytes: &[u8], _dest: SocketAddr) -> std::io::Result<()> {
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(bytes.to_vec());
        Ok(())
    }

    fn recv_from(&mut self, _buf: &mut [u8]) -> std::io::Result<Option<(usize, SocketAddr)>> {
        Ok(None)
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(SocketAddr::from(([10, 250, 0, 2], 40_000)))
    }
}

/// The `forged-handshake` attacker: real `ConnectRequest` datagrams resent from forged sources in
/// [`FORGED_PREFIXES`] /24s at [`FORGED_PER_S`].
struct Forger {
    ep: EndpointId,
    requests: Vec<Vec<u8>>,
    rng: SoakRng,
    sent: u64,
}

impl Forger {
    fn new(net: &ImpairedNet, s: &Scenario) -> Option<Self> {
        let ep = net
            .add_endpoint(SocketAddr::from(([10, 250, 0, 1], 40_000)), LinkSpec::default())
            .ok()?;
        let mut rng = SoakRng::derive(s.seed, 4_242);
        let mut requests = Vec::new();
        for _ in 0..16 {
            let socket = CaptureSocket::default();
            let sent = socket.sent.clone();
            let Ok(c) = NetClient::<SoakGame, _>::connect(
                socket,
                server_addr(),
                client_config(
                    "forged".into(),
                    TimeoutPolicy::default(),
                    false,
                    false,
                    None,
                    None,
                ),
                &u32::MAX,
            ) else {
                continue;
            };
            let mut c = c.with_request_nonce(rng.next_u64());
            c.flush(0);
            if let Some(first) = sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .first()
            {
                requests.push(first.clone());
            }
        }
        (!requests.is_empty()).then_some(Self {
            ep,
            requests,
            rng,
            sent: 0,
        })
    }

    /// Sends the requests due in one driver step.
    fn step(&mut self, net: &ImpairedNet) {
        for _ in 0..FORGED_PER_S * STEP_MS / 1000 {
            let prefix = self.rng.up_to(FORGED_PREFIXES - 1);
            let host = 1 + self.rng.up_to(253) as u8;
            let port = 1_024 + self.rng.up_to(64_000) as u16;
            let from = SocketAddr::from(([20 + (prefix / 256) as u8, (prefix % 256) as u8, 7, host], port));
            let i = self.rng.up_to(self.requests.len() as u64 - 1) as usize;
            net.send_forged(self.ep, from, &self.requests[i], server_addr());
            self.sent += 1;
        }
    }
}

/// Runs one scenario to the end with the engine's own policies ([`Knobs::default`]). Fails only
/// when the network cannot place an endpoint.
pub fn run(s: &Scenario) -> std::io::Result<RunRecord> {
    run_with(s, &Knobs::default())
}

/// [`run`] with the endpoints' policies replaced by `knobs`: how a test shows a bar failing on a
/// broken policy. The record does not say which knobs it ran with (its `reproduce` command reruns
/// the scenario with the engine's own), so keep such records out of a `result.json`.
pub fn run_with(s: &Scenario, knobs: &Knobs) -> std::io::Result<RunRecord> {
    run_inner(s, knobs, &[])
}

/// [`run`] with a client capture per honest client (ADR-8221): `taps[i]` captures client `i`
/// (fewer taps than clients capture the first ones). The run is the same with or without them.
pub fn run_with_captures(s: &Scenario, taps: &[CaptureTap]) -> std::io::Result<RunRecord> {
    run_inner(s, &Knobs::default(), taps)
}

fn run_inner(s: &Scenario, knobs: &Knobs, taps: &[CaptureTap]) -> std::io::Result<RunRecord> {
    let _span = tracing::info_span!("net_soak_run", label = %s.label()).entered();
    let wall = Instant::now();
    CLIENT_GAPS.with(|g| g.borrow_mut().clear());
    let net = match s.wire {
        WireMode::Virtual => ImpairedNet::new(s.seed),
        WireMode::Udp => ImpairedNet::with_real_udp(s.seed),
    };
    let server_link = s.profile.server_link(s.mode == ServerMode::Listen);
    let server_ep = net.add_endpoint(server_addr(), server_link.clone())?;
    let links: Vec<LinkSpec> = (0..s.clients)
        .map(|i| s.profile.client_link(i, s.duration_ms))
        .collect();
    let mut clients: Vec<Client> = Vec::with_capacity(s.clients);
    for (i, link) in links.iter().enumerate() {
        let addr = s.profile.client_addr(i).unwrap_or_else(|| client_addr(i));
        let ep = net.add_endpoint(addr, link.clone())?;
        let mut c = Client::new(i, ep, link.clone(), net.clone(), s, *knobs);
        c.capture = taps.get(i).cloned();
        clients.push(c);
    }
    let mut hostile = if (s.profile.hostile || s.profile.flood_peer) && !clients.is_empty() {
        Hostile::new(&net, s, clients[0].ep, !s.profile.hostile, knobs.hostile_relay)
    } else {
        None
    };
    let mut forger = if s.profile.forged_handshake {
        Forger::new(&net, s)
    } else {
        None
    };
    let mut server = Server::new(&net, server_ep, s, clients.iter().map(|c| c.ep).collect(), knobs);
    let mut run = RunState::new(server_link);
    // The server wakes once per tick, at the tick's start (k * 1000 / 30 ms), like a paced host.
    let mut server_wakes: u64 = 0;
    let mut now = 0;
    while now <= s.duration_ms {
        net.advance_to_ms(now);
        if let Some(f) = forger.as_mut() {
            f.step(&net);
        }
        for c in clients.iter_mut() {
            if c.frame_due(now) {
                c.before_frame(now, &mut run);
                c.frame(now, &mut run);
            }
        }
        if let Some(h) = hostile.as_mut()
            && now.is_multiple_of(FRAME_MS)
        {
            h.frame(now);
        }
        if now >= server_wakes * 1000 / TICK_HZ {
            server.iteration(now, &links, &mut run);
            server_wakes = now * TICK_HZ / 1000 + 1;
        }
        now += STEP_MS;
    }
    let end = s.duration_ms;
    for c in clients.iter_mut() {
        c.finish(end);
    }
    server.finish(end);
    let client_gaps = CLIENT_GAPS.with(|g| std::mem::take(&mut *g.borrow_mut()));
    Ok(RunRecord {
        scenario: s.clone(),
        client_net: clients.iter().map(|c| net.stats(c.ep)).collect(),
        server_net: net.stats(server_ep),
        links,
        ends: run.ends,
        reconnects: run.reconnects,
        client_gaps,
        clients: clients.into_iter().map(|c| c.rec).collect(),
        server: server.rec,
        hostile: hostile.map(|h| h.rec),
        wall_ms: wall.elapsed().as_millis() as u64,
        forged_sent: forger.map_or(0, |f| f.sent),
        wire: net.wire_stats(),
        rss_kib: process_rss_kib(),
    })
}

/// `VmRSS` and `VmHWM` of this process, KiB (Linux only).
fn process_rss_kib() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse::<u64>().ok())
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}

/// What replaying one client's capture gave ([`replay_client`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ClientReplay {
    /// Client index.
    pub client: usize,
    /// Frames replayed (the capture's frame markers).
    pub frames: u64,
    /// Sessions (connections) in the capture.
    pub sessions: usize,
    /// The replayed client's record: corrections, inputs, snapshots.
    pub record: ClientRecord,
    /// The replay's final output digest.
    pub digest: u64,
    /// The capture's final output digest (its last frame marker).
    pub captured_digest: Option<u64>,
    /// The first record where the replay differs from the capture (frame digests, input
    /// stamps, receive and flush times, ends), with the session index; `None` when it matches.
    pub divergence: Option<(usize, regia_net::capture::CaptureDivergence)>,
}

impl ClientReplay {
    /// True when the replay reproduced the capture exactly.
    pub fn matches(&self) -> bool {
        self.divergence.is_none() && self.captured_digest == Some(self.digest)
    }
}

/// The scenario of a capture the soak wrote: its label is the scenario as JSON (see
/// [`capture_label`]).
///
/// # Errors
///
/// A message when the label is not one.
pub fn capture_scenario(file: &CaptureFile) -> Result<(Scenario, usize), String> {
    #[derive(Deserialize)]
    struct Label {
        scenario: Scenario,
        client: usize,
    }
    serde_json::from_str::<Label>(&file.label)
        .map(|l| (l.scenario, l.client))
        .map_err(|e| format!("the capture's label is not a net-soak scenario: {e}"))
}

/// The label the soak writes into a client capture: the scenario and client index as JSON, so a
/// replay rebuilds the client exactly.
pub fn capture_label(s: &Scenario, client: usize) -> String {
    serde_json::json!({ "scenario": s, "client": client }).to_string()
}

/// Replays client `idx` of scenario `s` from `file` (ADR-8221): the soak client is rebuilt with
/// the scenario's seed and link, its `NetClient`s read the captured sessions instead of the
/// network, and its frames run at the captured frame times. The replay's own capture is compared
/// with `file` record by record.
///
/// # Errors
///
/// `capture_empty` when the file holds no session, or a refused protocol.
pub fn replay_client(s: &Scenario, idx: usize, file: &CaptureFile) -> Result<ClientReplay, CaptureError> {
    let sessions = file.sessions();
    if sessions.is_empty() {
        return Err(CaptureError::Empty);
    }
    let frames: Vec<(u64, u64)> = file
        .records
        .iter()
        .filter_map(|r| match r {
            CaptureRecord::Frame { at_ms, digest } => Some((*at_ms, *digest)),
            _ => None,
        })
        .collect();
    // The replay's own ring holds everything the capture held, with room to spare.
    let need: usize = file.records.iter().map(CaptureRecord::ring_bytes).sum();
    let ring = (need + need / 4 + (64 << 10)).clamp(
        regia_net::capture::MIN_CAPTURE_RING_BYTES,
        regia_net::capture::MAX_CAPTURE_RING_BYTES,
    );
    let tap = CaptureTap::new(ring)?;
    let net = ImpairedNet::new(s.seed);
    let link = s.profile.client_link(idx, s.duration_ms);
    let addr = s.profile.client_addr(idx).unwrap_or_else(|| client_addr(idx));
    let ep = net
        .add_endpoint(addr, link.clone())
        .map_err(|e| CaptureError::Io(format!("replay endpoint: {e}")))?;
    let mut c = Client::new(idx, ep, link, net, s, Knobs::default());
    c.capture = Some(tap.clone());
    c.replay = Some(sessions.iter().cloned().collect());
    // No server runs in a replay: the staleness split (ADR-10096) reads the scenario's server link,
    // and with no tick or send times it counts each snapshot as sent when it is judged.
    let mut run = RunState::new(s.profile.server_link(s.mode == ServerMode::Listen));
    for &(at, _) in &frames {
        c.before_frame(at, &mut run);
        c.frame(at, &mut run);
    }
    let replayed = tap.lock().sessions();
    let mut divergence = None;
    for (i, captured) in sessions.iter().enumerate() {
        let mine = replayed.get(i).map_or(&[][..], Vec::as_slice);
        if let Some(d) = regia_net::capture::compare_sessions(captured, mine) {
            divergence = Some((i, d));
            break;
        }
    }
    if divergence.is_none() && replayed.len() > sessions.len() {
        divergence = Some((
            sessions.len(),
            regia_net::capture::CaptureDivergence {
                index: 0,
                captured: None,
                replayed: Some("an extra session".into()),
            },
        ));
    }
    Ok(ClientReplay {
        client: idx,
        frames: frames.len() as u64,
        sessions: sessions.len(),
        digest: c.digest,
        captured_digest: frames.last().map(|f| f.1),
        record: c.rec,
        divergence,
    })
}
