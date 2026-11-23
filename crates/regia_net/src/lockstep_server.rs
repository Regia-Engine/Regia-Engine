// SPDX-License-Identifier: MPL-2.0
//! The headless lockstep server game type (spec 120 § 7.1; ADR-8558): a dedicated process that
//! is the frame authority of a lockstep match for up to `max_seats` seats, with an optional
//! observer simulation on its own thread that votes in the hash quorum. No ECS: it builds in the
//! `regia_net` core (`--no-default-features`), and the dedicated-server host wraps it as a
//! `HostGame` (`server_host::lockstep_game`, feature `ecs`).
//!
//! This is host-side code (ADR-8656): it owns a thread and reads the wall clock for the observer's
//! step-time rows, so it lives outside the `lockstep` module the determinism scan (`cargo xtask
//! det-scan`) holds to the lockstep rules, and it drives the deterministic core only through
//! [`LockstepHost`]'s public calls (`receive`, `poll_frame`, `outgoing`, `report_hash`, `revote`,
//! `set_observer_voting`, `poll_event`, `status`). Nothing it measures goes into a frame. The path
//! `regia_net::lockstep::server` names the same module.
//!
//! - [`LockstepServer`]: the [`LockstepHost`] in [`HostRole::Observer`] (with an observer) or
//!   [`HostRole::Relay`] (without), the seat cap, the observer thread and the counters of
//!   [`LockstepServerStatus`].
//! - [`LockstepObserver`]: the game's simulation as the observer steps it: one frame at a time,
//!   then the state hash.
//! - [`LockstepServerSettings`]: the `[lockstep]` section of `server.toml`.
//!
//! **The seats never wait for the observer.** Frames are built on the host's clock alone; the
//! observer gets a copy of each frame through a channel and its hashes come back the same way.
//! An observer more than `observer_max_lag_ticks` behind leaves the quorum (counted in
//! `observer.dropouts`), keeps stepping, and votes again once it is back within the rejoin lag
//! (`observer.rejoins`). One more than `observer_max_backlog_ticks` behind, the server stops
//! feeding it and it is detached for the rest of the match (`observer.detached`): a simulation that
//! stalled must not hold every frame of the match in memory.
//!
//! **An outvoted observer takes the majority's state** (ADR-8855) when its simulation can load one
//! ([`LockstepObserver::loads_state`]): the host's [`HostEvent::ResyncHost`] names the majority
//! seat and the checkpoint, the game delivers that seat's state with
//! [`LockstepServer::load_observer_state`] (the seat got [`crate::lockstep::SeatEvent::ProvideState`]),
//! the observer loads it, replays the frames after it, and votes again (`observer.resyncs`). Its
//! votes are not counted while the transfer is pending. An observer whose simulation cannot load
//! a state, or refuses the one it got, leaves the quorum for the match (`observer.diverged`).
//!
//! ```
//! use regia_net::lockstep::server::{LockstepObserver, LockstepServer, LockstepServerSettings};
//! use regia_net::lockstep::{LockstepFrame, LockstepSeat, SeatId};
//! use regia_net::NetGame;
//!
//! struct Game;
//! impl NetGame for Game {
//!     type Message = Vec<u8>;
//!     type Input = u8;
//!     type JoinData = ();
//!     const GAME_ID: &'static str = "doc.lockstep.server";
//!     const PROTOCOL_VERSION: u32 = 1;
//! }
//! // The observer's simulation: a running sum of the inputs.
//! #[derive(Default)]
//! struct Sum(u64);
//! impl LockstepObserver<u8> for Sum {
//!     fn step(&mut self, frame: &LockstepFrame<u8>) {
//!         for (_, input) in &frame.inputs {
//!             self.0 += input.as_ref().map_or(0, |i| u64::from(*i));
//!         }
//!     }
//!     fn state_hash(&self, tick: u64) -> u64 {
//!         self.0 ^ tick
//!     }
//! }
//!
//! let settings = LockstepServerSettings { enabled: true, ..LockstepServerSettings::default() };
//! let mut server = LockstepServer::<Game>::new(&settings, 60, Some(Box::new(Sum::default())))
//!     .expect("server");
//! server.add_seat(SeatId(0)).expect("seat");
//! server.start(0);
//! let mut seat = LockstepSeat::<Game>::new(server.config().clone(), SeatId(0));
//! for now_ms in (0..500u64).step_by(16) {
//!     let _ = seat.submit_input(&1);
//!     for out in seat.outgoing(now_ms) {
//!         server.receive(SeatId(0), out.lane, &out.bytes, now_ms).expect("seat bytes");
//!     }
//!     server.tick(now_ms);
//!     server.sync_observer();
//!     for (_, out) in server.outgoing(now_ms) {
//!         seat.receive(out.lane, &out.bytes, now_ms).expect("host bytes");
//!     }
//!     while seat.next_frame().is_some() {}
//! }
//! let status = server.status(500);
//! assert!(status.tick > 20);
//! assert_eq!(status.observer.as_ref().map(|o| o.voting), Some(true));
//! ```

use std::collections::{BTreeSet, VecDeque};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::lockstep::{
    DesyncVerdict, HostEvent, HostRole, Lane, LockstepConfig, LockstepError, LockstepFrame, LockstepHost,
    LockstepStatus, Outgoing, SeatId, Voter,
};
use crate::payload::NetGame;

/// Seats a lockstep server takes by default (DO27's all-seat load test; its 22-seat match cap is
/// the same setting set lower).
pub const DEFAULT_MAX_SEATS: u32 = 40;
/// The largest `max_seats`: what one frame can list.
pub const MAX_SEATS_LIMIT: u32 = 4096;
/// Ticks the observer may trail the newest frame before it leaves the quorum, by default (one
/// second at 60 Hz; half the default vote deadline, so a checkpoint is never held long for it).
pub const DEFAULT_OBSERVER_MAX_LAG_TICKS: u32 = 60;
/// Ticks the observer may trail before it is detached for the match, by default (the default
/// frame log: ten seconds at 60 Hz).
pub const DEFAULT_OBSERVER_MAX_BACKLOG_TICKS: u32 = 600;
/// Step times the observer keeps for its percentiles (a minute at 60 Hz).
const STEP_SAMPLES: usize = 3_600;
/// A step slower than this is a stall, microseconds (100 ms).
const STALL_US: u64 = 100_000;
/// Observer hashes kept for a self-check vote.
const OBSERVER_HASHES: usize = 1_024;
/// Verdicts kept for the status.
const MAX_VERDICTS: usize = 16;

/// `[lockstep]` in `server.toml` (ADR-8558): the dedicated server hosts a lockstep match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LockstepServerSettings {
    /// The server runs the lockstep game type (`HostGame`) instead of the project's own game.
    pub enabled: bool,
    /// Seats the match takes at most (1..=4096).
    pub max_seats: u32,
    /// The input delay the match starts with, ticks (1..=1000).
    pub input_delay_ticks: u32,
    /// The server steps its own simulation as an observer and votes in the hash quorum.
    pub observer: bool,
    /// A state hash is voted on every this many ticks (0: never; at most 3600).
    pub checkpoint_ticks: u32,
    /// Ticks the observer may trail the newest frame before it leaves the quorum (1..=3600).
    pub observer_max_lag_ticks: u32,
    /// Ticks the observer may trail before it is detached for the match
    /// (`observer_max_lag_ticks`..=65536).
    pub observer_max_backlog_ticks: u32,
}

impl Default for LockstepServerSettings {
    fn default() -> Self {
        let config = LockstepConfig::default();
        Self {
            enabled: false,
            max_seats: DEFAULT_MAX_SEATS,
            input_delay_ticks: u32::from(config.initial_delay_ticks),
            observer: true,
            checkpoint_ticks: config.hash_every,
            observer_max_lag_ticks: DEFAULT_OBSERVER_MAX_LAG_TICKS,
            observer_max_backlog_ticks: DEFAULT_OBSERVER_MAX_BACKLOG_TICKS,
        }
    }
}

/// One `[lockstep]` field that does not parse or is out of range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Error)]
#[error("{field}: expected {expected}")]
pub struct LockstepFieldError {
    /// `lockstep.<key>`.
    pub field: String,
    /// What the field takes.
    pub expected: String,
}

impl LockstepServerSettings {
    /// The `server.toml` keys of the section, in file order.
    pub const KEYS: [&'static str; 7] = [
        "enabled",
        "max_seats",
        "input_delay_ticks",
        "observer",
        "checkpoint_ticks",
        "observer_max_lag_ticks",
        "observer_max_backlog_ticks",
    ];

    /// Applies one `[lockstep]` key. `Ok(false)`: the key is not one of [`Self::KEYS`] (the
    /// caller warns and ignores it, like any unknown `server.toml` key).
    pub fn apply_key(&mut self, key: &str, value: &toml::Value) -> Result<bool, LockstepFieldError> {
        let bad = |expected: &str| LockstepFieldError {
            field: format!("lockstep.{key}"),
            expected: expected.to_owned(),
        };
        let boolean = |out: &mut bool| match value.as_bool() {
            Some(b) => {
                *out = b;
                Ok(true)
            }
            None => Err(bad("true or false")),
        };
        let int = |out: &mut u32| match value.as_integer().map(u32::try_from) {
            Some(Ok(n)) => {
                *out = n;
                Ok(true)
            }
            _ => Err(bad("a non-negative integer")),
        };
        match key {
            "enabled" => boolean(&mut self.enabled),
            "observer" => boolean(&mut self.observer),
            "max_seats" => int(&mut self.max_seats),
            "input_delay_ticks" => int(&mut self.input_delay_ticks),
            "checkpoint_ticks" => int(&mut self.checkpoint_ticks),
            "observer_max_lag_ticks" => int(&mut self.observer_max_lag_ticks),
            "observer_max_backlog_ticks" => int(&mut self.observer_max_backlog_ticks),
            _ => Ok(false),
        }
    }

    /// The range rules, one error per failing field.
    pub fn validate(&self) -> Vec<LockstepFieldError> {
        let mut out = Vec::new();
        let mut bad = |key: &str, expected: String| {
            out.push(LockstepFieldError {
                field: format!("lockstep.{key}"),
                expected,
            });
        };
        if !(1..=MAX_SEATS_LIMIT).contains(&self.max_seats) {
            bad("max_seats", format!("1..={MAX_SEATS_LIMIT}"));
        }
        if !(1..=1_000).contains(&self.input_delay_ticks) {
            bad("input_delay_ticks", "1..=1000 ticks".into());
        }
        if self.checkpoint_ticks > 3_600 {
            bad("checkpoint_ticks", "0..=3600 ticks (0 = no hash votes)".into());
        }
        if !(1..=3_600).contains(&self.observer_max_lag_ticks) {
            bad("observer_max_lag_ticks", "1..=3600 ticks".into());
        }
        if !(self.observer_max_lag_ticks..=65_536).contains(&self.observer_max_backlog_ticks) {
            bad(
                "observer_max_backlog_ticks",
                "observer_max_lag_ticks..=65536 ticks".into(),
            );
        }
        out
    }

    /// Reads the `[lockstep]` table of a parsed `server.toml` over the defaults, then validates.
    /// Unknown keys are returned as warnings. No table: the defaults (lockstep off).
    pub fn from_table(table: &toml::Table) -> Result<(Self, Vec<String>), Vec<LockstepFieldError>> {
        let mut settings = Self::default();
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        match table.get("lockstep") {
            None => {}
            Some(toml::Value::Table(t)) => {
                for (k, v) in t {
                    match settings.apply_key(k, v) {
                        Ok(true) => {}
                        Ok(false) => {
                            warnings.push(format!("server.toml: unknown key `lockstep.{k}` ignored"))
                        }
                        Err(e) => errors.push(e),
                    }
                }
            }
            Some(_) => errors.push(LockstepFieldError {
                field: "lockstep".into(),
                expected: "a [lockstep] table".into(),
            }),
        }
        if errors.is_empty() {
            errors = settings.validate();
        }
        if errors.is_empty() {
            Ok((settings, warnings))
        } else {
            Err(errors)
        }
    }

    /// The section as `server.toml` text (`[lockstep]` and one line per key); it parses back to
    /// these settings.
    pub fn to_toml(&self) -> String {
        format!(
            "[lockstep]\nenabled = {}\nmax_seats = {}\ninput_delay_ticks = {}\nobserver = {}\ncheckpoint_ticks = {}\nobserver_max_lag_ticks = {}\nobserver_max_backlog_ticks = {}\n",
            self.enabled,
            self.max_seats,
            self.input_delay_ticks,
            self.observer,
            self.checkpoint_ticks,
            self.observer_max_lag_ticks,
            self.observer_max_backlog_ticks,
        )
    }

    /// The match settings at `tick_hz`: [`LockstepConfig::at_rate`] with the section's delay and
    /// checkpoint interval (sanitized).
    pub fn lockstep_config(&self, tick_hz: u32) -> LockstepConfig {
        let mut config = LockstepConfig::at_rate(tick_hz);
        let delay = u16::try_from(self.input_delay_ticks).unwrap_or(u16::MAX);
        config.initial_delay_ticks = delay;
        config.max_delay_ticks = config.max_delay_ticks.max(delay);
        config.hash_every = self.checkpoint_ticks;
        config.sanitized()
    }
}

/// The game's simulation as the server's observer steps it, on its own thread.
pub trait LockstepObserver<I>: Send {
    /// Steps one frame. Frames come in order, each once, from tick 0.
    fn step(&mut self, frame: &LockstepFrame<I>);
    /// The state hash after the frame at `tick` was stepped (the same function every seat votes
    /// with).
    fn state_hash(&self, tick: u64) -> u64;
    /// Whether [`Self::load_state`] can take a state transfer. Asked once, when the server
    /// starts. `false` (the default): an outvoted observer leaves the quorum for the match.
    fn loads_state(&self) -> bool {
        false
    }
    /// Loads `state`, the game's bytes of a majority seat's state after the frame at `tick`
    /// (what that seat sends for [`crate::lockstep::SeatEvent::ProvideState`]). The frames after
    /// `tick` follow through [`Self::step`]. Returns `false` when the bytes do not load; the
    /// observer then leaves the quorum for the match. The default loads nothing.
    fn load_state(&mut self, tick: u64, state: &[u8]) -> bool {
        let _ = (tick, state);
        false
    }
}

/// Why a [`LockstepServer`] call was refused. [`Self::code`] is the agent-protocol code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LockstepServerError {
    /// Every seat is taken.
    #[error("the lockstep match is full ({max} seats)")]
    SeatsFull {
        /// `max_seats`.
        max: u32,
    },
    /// `[lockstep] observer = true` and the game gave no observer simulation.
    #[error("[lockstep] observer is on and the game has no observer simulation")]
    ObserverMissing,
    /// The observer thread could not be started.
    #[error("the observer thread did not start: {0}")]
    ObserverThread(String),
    /// A settings field is out of range.
    #[error("{0}")]
    Settings(LockstepFieldError),
    /// [`LockstepServer::load_observer_state`] without a pending transfer for that tick (the host
    /// did not order one, it was ordered again for a newer checkpoint, or it was already taken).
    #[error("the observer has no state transfer pending for tick {tick}")]
    ObserverNoTransfer {
        /// The tick the state was for.
        tick: u64,
    },
    /// The host no longer holds the frames after the transfer's tick, so the observer cannot
    /// replay to the live edge; the transfer is ordered again at its deadline.
    #[error("the frames after tick {tick} are no longer held")]
    ObserverStateTooOld {
        /// The tick the state was for.
        tick: u64,
    },
}

impl LockstepServerError {
    /// The agent-protocol error code (lowercase ascii, digits and `_`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::SeatsFull { .. } => "lockstep_seats_full",
            Self::ObserverMissing => "lockstep_observer_missing",
            Self::ObserverThread(_) => "lockstep_observer_thread",
            Self::Settings(_) => "lockstep_settings_invalid",
            Self::ObserverNoTransfer { .. } => "lockstep_observer_no_transfer",
            Self::ObserverStateTooOld { .. } => "lockstep_observer_state_too_old",
        }
    }
}

/// The observer's rows of [`LockstepServerStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ObserverStatus {
    /// The observer still gets frames (it was not detached).
    pub attached: bool,
    /// Its hash counts in the quorum right now.
    pub voting: bool,
    /// The next tick it steps.
    pub next_tick: u64,
    /// Ticks it trails the newest frame built.
    pub lag_ticks: u64,
    /// The largest lag seen.
    pub lag_max_ticks: u64,
    /// Frames stepped.
    pub frames_stepped: u64,
    /// Times it left the quorum for falling behind.
    pub dropouts: u64,
    /// Times it came back into the quorum.
    pub rejoins: u64,
    /// It fell behind past the backlog cap and was detached for the match.
    pub detached: bool,
    /// The quorum outvoted it and it could not take the majority's state; it votes no more this
    /// match.
    pub diverged: bool,
    /// State transfers it took after the quorum outvoted it (ADR-8855).
    pub resyncs: u64,
    /// The checkpoint of the state transfer it waits for (`None`: none pending).
    pub transfer_tick: Option<u64>,
    /// Self-check requests it answered with its live hash.
    pub self_checks: u64,
    /// Step time p50 over the last minute of steps, microseconds (measured, never asserted).
    pub step_us_p50: u64,
    /// Step time p99, microseconds.
    pub step_us_p99: u64,
    /// The slowest step this match, microseconds.
    pub step_us_max: u64,
    /// Steps slower than 100 ms this match.
    pub stalls: u64,
}

/// What `server.lockstep_status` reports (ADR-8558).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LockstepServerStatus {
    /// The next tick to build.
    pub tick: u64,
    /// Seats in the match.
    pub seats: u32,
    /// `max_seats`.
    pub max_seats: u32,
    /// Frames built.
    pub frames_built: u64,
    /// The observer, when the server has one.
    pub observer: Option<ObserverStatus>,
    /// Desync verdicts so far.
    pub verdicts: u64,
    /// The newest verdicts, oldest first (at most 16).
    pub recent_verdicts: Vec<DesyncVerdict>,
    /// Seats refused because the match was full.
    pub seats_refused: u64,
    /// The host's own rows (`net.lockstep`).
    pub lockstep: LockstepStatus,
}

fn verdict_kind(kind: crate::lockstep::VerdictKind) -> &'static str {
    match kind {
        crate::lockstep::VerdictKind::SelfHealed => "self_healed",
        crate::lockstep::VerdictKind::Diverged => "diverged",
        crate::lockstep::VerdictKind::Split => "split",
        crate::lockstep::VerdictKind::Unresolved => "unresolved",
    }
}

fn voters_json(voters: &[crate::lockstep::Voter]) -> String {
    let names: Vec<String> = voters.iter().map(|v| format!("\"{v}\"")).collect();
    format!("[{}]", names.join(","))
}

impl LockstepServerStatus {
    /// The rows as one JSON object: the `lockstep` object of the host's `/health` body and the
    /// result of the agent's `server.lockstep_status` (no JSON library: the core has none).
    pub fn health_json(&self) -> String {
        let observer = self.observer.as_ref().map_or_else(
            || "null".to_owned(),
            |o| {
                format!(
                    "{{\"voting\":{},\"attached\":{},\"next_tick\":{},\"lag_ticks\":{},\"lag_max_ticks\":{},\"frames_stepped\":{},\"dropouts\":{},\"rejoins\":{},\"detached\":{},\"diverged\":{},\"resyncs\":{},\"transfer_tick\":{},\"self_checks\":{},\"step_us_p50\":{},\"step_us_p99\":{},\"step_us_max\":{},\"stalls\":{}}}",
                    o.voting,
                    o.attached,
                    o.next_tick,
                    o.lag_ticks,
                    o.lag_max_ticks,
                    o.frames_stepped,
                    o.dropouts,
                    o.rejoins,
                    o.detached,
                    o.diverged,
                    o.resyncs,
                    o.transfer_tick
                        .map_or_else(|| "null".to_owned(), |t| t.to_string()),
                    o.self_checks,
                    o.step_us_p50,
                    o.step_us_p99,
                    o.step_us_max,
                    o.stalls,
                )
            },
        );
        let last = self.recent_verdicts.last().map_or_else(
            || "null".to_owned(),
            |v| {
                format!(
                    "{{\"tick\":{},\"kind\":\"{}\",\"agreeing\":{},\"diverged\":{},\"abstained\":{}}}",
                    v.tick,
                    verdict_kind(v.kind),
                    voters_json(&v.agreeing),
                    voters_json(&v.diverged),
                    voters_json(&v.abstained),
                )
            },
        );
        let l = &self.lockstep;
        format!(
            "{{\"tick\":{},\"seats\":{},\"max_seats\":{},\"frames_built\":{},\"seats_refused\":{},\"delay_ticks\":{},\"checkpoints_agreed\":{},\"last_agreed_tick\":{},\"desyncs\":{},\"resyncs\":{},\"verdicts\":{},\"last_verdict\":{},\"observer\":{}}}",
            self.tick,
            self.seats,
            self.max_seats,
            self.frames_built,
            self.seats_refused,
            l.delay_ticks,
            l.checkpoints_agreed,
            l.last_agreed_tick
                .map_or_else(|| "null".to_owned(), |t| t.to_string()),
            l.desyncs,
            l.resyncs,
            self.verdicts,
            last,
            observer,
        )
    }

    /// The console's `lockstep` lines.
    pub fn console_lines(&self) -> Vec<String> {
        let l = &self.lockstep;
        let mut out = vec![format!(
            "lockstep tick={} seats={}/{} delay={} agreed={} verdicts={} desyncs={} resyncs={}",
            self.tick,
            self.seats,
            self.max_seats,
            l.delay_ticks,
            l.checkpoints_agreed,
            self.verdicts,
            l.desyncs,
            l.resyncs
        )];
        if let Some(o) = &self.observer {
            out.push(format!(
                "observer voting={} lag={} lag_max={} dropouts={} rejoins={} detached={} diverged={} resyncs={} step_us_p99={} stalls={}",
                o.voting,
                o.lag_ticks,
                o.lag_max_ticks,
                o.dropouts,
                o.rejoins,
                o.detached,
                o.diverged,
                o.resyncs,
                o.step_us_p99,
                o.stalls
            ));
        }
        if let Some(v) = self.recent_verdicts.last() {
            let names: Vec<String> = v.diverged.iter().map(ToString::to_string).collect();
            out.push(format!(
                "last verdict tick={} kind={} diverged=[{}]",
                v.tick,
                verdict_kind(v.kind),
                names.join(",")
            ));
        }
        out
    }
}

/// What the observer thread and the server share.
#[derive(Debug, Default)]
struct Shared {
    /// Frames handed to the thread.
    sent: u64,
    /// Frames the thread stepped.
    stepped: u64,
    /// The next tick the thread steps.
    next_tick: u64,
    /// Steps the thread may still take (`None`: no limit). The test hook of [`ObserverThrottle`].
    budget: Option<u64>,
    /// Stop now (the server is dropped or detached the observer).
    stop: bool,
    /// The thread ended (stopped, or its simulation panicked).
    dead: bool,
    /// State loads handed to the thread.
    loads_sent: u64,
    /// State loads the thread finished (loaded and replayed, or refused).
    loads_done: u64,
    step_us: VecDeque<u64>,
    step_us_max: u64,
    stalls: u64,
}

type SharedPair = Arc<(Mutex<Shared>, Condvar)>;

fn lock(shared: &SharedPair) -> MutexGuard<'_, Shared> {
    shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A hook that limits how many frames the observer thread may step: a test (or an operator)
/// slows the observer without a clock. Cloneable; every clone controls the same observer.
#[derive(Clone)]
pub struct ObserverThrottle {
    shared: SharedPair,
}

impl std::fmt::Debug for ObserverThrottle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObserverThrottle")
            .field("budget", &lock(&self.shared).budget)
            .finish()
    }
}

impl ObserverThrottle {
    fn set(&self, budget: Option<u64>) {
        lock(&self.shared).budget = budget;
        self.shared.1.notify_all();
    }

    /// The observer steps no further frame until [`Self::allow`] or [`Self::release`].
    pub fn hold(&self) {
        self.set(Some(0));
    }

    /// The observer may step `frames` more frames, then holds again.
    pub fn allow(&self, frames: u64) {
        self.set(Some(frames));
    }

    /// No limit.
    pub fn release(&self) {
        self.set(None);
    }
}

/// Marks the thread dead when its loop ends, however it ends (a panicking simulation included),
/// so [`LockstepServer::sync_observer`] never waits for a thread that is gone.
struct DeadOnDrop(SharedPair);

impl Drop for DeadOnDrop {
    fn drop(&mut self) {
        lock(&self.0).dead = true;
        self.0.1.notify_all();
    }
}

/// What the server hands the observer thread, in order.
enum ObserverMsg<I> {
    /// The next frame to step.
    Frame(LockstepFrame<I>),
    /// A state transfer: load `state` as the state after `tick`, then step `replay` (the frames
    /// after `tick` the server had handed over before this message).
    Load {
        tick: u64,
        state: Vec<u8>,
        replay: Vec<LockstepFrame<I>>,
    },
}

/// What the observer thread reports back, in order.
enum ObserverReport {
    /// Its hash after it stepped the frame at the tick live.
    Hash(u64, u64),
    /// A state transfer for the tick finished: its hash at the tick after the load and its hash
    /// after each frame it replayed (ascending by tick), or `None` when the simulation refused the
    /// bytes.
    Loaded(u64, Option<(u64, Vec<(u64, u64)>)>),
}

fn observer_loop<I>(
    mut sim: Box<dyn LockstepObserver<I>>,
    frames: Receiver<ObserverMsg<I>>,
    hashes: Sender<ObserverReport>,
    shared: SharedPair,
) {
    let _dead = DeadOnDrop(Arc::clone(&shared));
    while let Ok(msg) = frames.recv() {
        let frame = match msg {
            ObserverMsg::Frame(frame) => frame,
            ObserverMsg::Load { tick, state, replay } => {
                let loaded = {
                    let _span = tracing::trace_span!("lockstep_observer_load", tick).entered();
                    if sim.load_state(tick, &state) {
                        let hash = sim.state_hash(tick);
                        // Every replayed tick's hash goes back: the host's probe ring still
                        // holds the hashes the observer stepped before the load (ADR-10147).
                        let replayed: Vec<(u64, u64)> = replay
                            .iter()
                            .map(|frame| {
                                sim.step(frame);
                                (frame.tick, sim.state_hash(frame.tick))
                            })
                            .collect();
                        Some((hash, replayed))
                    } else {
                        None
                    }
                };
                let took = loaded.is_some();
                if hashes.send(ObserverReport::Loaded(tick, loaded)).is_err() {
                    return;
                }
                let mut s = lock(&shared);
                if took {
                    let next = replay.last().map_or(tick + 1, |f| f.tick + 1);
                    s.next_tick = s.next_tick.max(next);
                }
                s.loads_done += 1;
                drop(s);
                shared.1.notify_all();
                continue;
            }
        };
        {
            let mut s = lock(&shared);
            while !s.stop && s.budget == Some(0) {
                s = shared
                    .1
                    .wait(s)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if s.stop {
                return;
            }
            if let Some(b) = s.budget.as_mut() {
                *b -= 1;
            }
        }
        let started = Instant::now();
        {
            let _span = tracing::trace_span!("lockstep_observer_step", tick = frame.tick).entered();
            sim.step(&frame);
        }
        let hash = sim.state_hash(frame.tick);
        let us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        if hashes.send(ObserverReport::Hash(frame.tick, hash)).is_err() {
            return;
        }
        let mut s = lock(&shared);
        s.stepped += 1;
        s.next_tick = frame.tick + 1;
        if s.step_us.len() >= STEP_SAMPLES {
            s.step_us.pop_front();
        }
        s.step_us.push_back(us);
        s.step_us_max = s.step_us_max.max(us);
        if us > STALL_US {
            s.stalls += 1;
        }
        drop(s);
        shared.1.notify_all();
    }
}

/// The observer thread as the server holds it.
struct ObserverThread<I> {
    frames: Option<Sender<ObserverMsg<I>>>,
    hashes: Receiver<ObserverReport>,
    shared: SharedPair,
    thread: Option<JoinHandle<()>>,
    voting: bool,
    detached: bool,
    diverged: bool,
    /// Its simulation can take a state transfer ([`LockstepObserver::loads_state`]).
    loads_state: bool,
    /// The checkpoint of the transfer the host ordered and the game has not delivered yet.
    transfer: Option<u64>,
    /// A delivered transfer the thread has not finished (its tick).
    loading: Option<u64>,
    /// Transfers taken.
    resyncs: u64,
    lag_max: u64,
    dropouts: u64,
    rejoins: u64,
    self_checks: u64,
    /// The newest hashes it reported, for a self-check vote.
    recent: VecDeque<(u64, u64)>,
}

impl<I> ObserverThread<I> {
    fn stop(&mut self) {
        lock(&self.shared).stop = true;
        self.shared.1.notify_all();
        self.frames = None;
    }
}

impl<I> Drop for ObserverThread<I> {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The nearest-rank sample at `per_mille` / 1000 of the sorted samples (integer arithmetic: this
/// module is lockstep code, ADR-8012 A-4).
fn percentile(samples: &VecDeque<u64>, per_mille: usize) -> u64 {
    let mut v: Vec<u64> = samples.iter().copied().collect();
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = ((v.len() - 1) * per_mille + 500) / 1000;
    v.get(i).copied().unwrap_or(0)
}

/// The lockstep server game type: a [`LockstepHost`] with a seat cap and an optional observer on
/// its own thread (ADR-8558).
///
/// Drive it once per host tick, like the host: [`Self::receive`] what the seats sent,
/// [`Self::tick`], then send everything [`Self::outgoing`] returns. [`Self::poll_event`] passes the
/// host's events on after the server has read them.
pub struct LockstepServer<G: NetGame> {
    host: LockstepHost<G>,
    settings: LockstepServerSettings,
    seats: BTreeSet<SeatId>,
    observer: Option<ObserverThread<G::Input>>,
    frames_built: u64,
    seats_refused: u64,
    verdicts: u64,
    recent_verdicts: VecDeque<DesyncVerdict>,
    events: VecDeque<HostEvent>,
}

impl<G: NetGame> std::fmt::Debug for LockstepServer<G> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockstepServer")
            .field("settings", &self.settings)
            .field("seats", &self.seats.len())
            .field("observer", &self.observer.is_some())
            .finish_non_exhaustive()
    }
}

impl<G: NetGame> LockstepServer<G> {
    /// A server for `settings` at `tick_hz`. With `observer` on in the settings the game must
    /// give its simulation; with it off a given simulation is dropped and the host relays.
    pub fn new(
        settings: &LockstepServerSettings,
        tick_hz: u32,
        observer: Option<Box<dyn LockstepObserver<G::Input>>>,
    ) -> Result<Self, LockstepServerError> {
        if let Some(e) = settings.validate().into_iter().next() {
            return Err(LockstepServerError::Settings(e));
        }
        let observer = match (settings.observer, observer) {
            (true, None) => return Err(LockstepServerError::ObserverMissing),
            (true, Some(sim)) => Some(Self::spawn(sim)?),
            (false, _) => None,
        };
        let role = if observer.is_some() {
            HostRole::Observer
        } else {
            HostRole::Relay
        };
        Ok(Self {
            host: LockstepHost::new(settings.lockstep_config(tick_hz), role),
            settings: *settings,
            seats: BTreeSet::new(),
            observer,
            frames_built: 0,
            seats_refused: 0,
            verdicts: 0,
            recent_verdicts: VecDeque::new(),
            events: VecDeque::new(),
        })
    }

    fn spawn(
        sim: Box<dyn LockstepObserver<G::Input>>,
    ) -> Result<ObserverThread<G::Input>, LockstepServerError> {
        let loads_state = sim.loads_state();
        let (frame_tx, frame_rx) = channel();
        let (hash_tx, hash_rx) = channel();
        let shared: SharedPair = Arc::default();
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("regia-lockstep-observer".into())
            .spawn(move || observer_loop(sim, frame_rx, hash_tx, thread_shared))
            .map_err(|e| LockstepServerError::ObserverThread(e.to_string()))?;
        Ok(ObserverThread {
            frames: Some(frame_tx),
            hashes: hash_rx,
            shared,
            thread: Some(thread),
            voting: true,
            detached: false,
            diverged: false,
            loads_state,
            transfer: None,
            loading: None,
            resyncs: 0,
            lag_max: 0,
            dropouts: 0,
            rejoins: 0,
            self_checks: 0,
            recent: VecDeque::new(),
        })
    }
}

impl<G: NetGame> LockstepServer<G> {
    /// The settings.
    pub fn settings(&self) -> &LockstepServerSettings {
        &self.settings
    }

    /// The match settings every seat must use.
    pub fn config(&self) -> &LockstepConfig {
        self.host.config()
    }

    /// The frame authority.
    pub fn host(&self) -> &LockstepHost<G> {
        &self.host
    }

    /// The frame authority, to call what the server does not wrap (catch-ups, resume tokens,
    /// the match end).
    pub fn host_mut(&mut self) -> &mut LockstepHost<G> {
        &mut self.host
    }

    /// Seats in the match.
    pub fn seat_count(&self) -> u32 {
        self.seats.len() as u32
    }

    /// Takes `seat` into the match (listed from the next frame built), unless every seat is
    /// taken. Adding a seat that is already in changes nothing.
    pub fn add_seat(&mut self, seat: SeatId) -> Result<(), LockstepServerError> {
        if !self.seats.contains(&seat) && self.seats.len() as u32 >= self.settings.max_seats {
            self.seats_refused += 1;
            return Err(LockstepServerError::SeatsFull {
                max: self.settings.max_seats,
            });
        }
        self.seats.insert(seat);
        self.host.add_seat(seat);
        Ok(())
    }

    /// Takes `seat` out of the match (listed as leaving in the next frame built).
    pub fn remove_seat(&mut self, seat: SeatId) {
        if self.seats.remove(&seat) {
            self.host.remove_seat(seat);
        }
    }

    /// Starts the match clock at `now_ms`.
    pub fn start(&mut self, now_ms: u64) {
        self.host.start(now_ms);
    }

    /// Takes what `seat` sent.
    pub fn receive(
        &mut self,
        seat: SeatId,
        lane: Lane,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<(), LockstepError> {
        self.host.receive(seat, lane, bytes, now_ms)
    }

    /// Builds every frame due at `now_ms`, hands each to the observer, takes the observer's
    /// hashes that are ready (never waiting for them) and moves it in or out of the quorum by its
    /// lag. Returns the frames built, oldest first.
    pub fn tick(&mut self, now_ms: u64) -> Vec<LockstepFrame<G::Input>> {
        let _span = tracing::trace_span!("lockstep_server_tick").entered();
        let mut built = Vec::new();
        while let Some(frame) = self.host.poll_frame(now_ms) {
            self.frames_built += 1;
            if let Some(obs) = self.observer.as_mut()
                && let Some(tx) = obs.frames.as_ref()
            {
                if tx.send(ObserverMsg::Frame(frame.clone())).is_ok() {
                    lock(&obs.shared).sent += 1;
                } else {
                    obs.frames = None;
                }
            }
            built.push(frame);
        }
        self.take_hashes();
        self.check_observer();
        self.take_events();
        built
    }

    /// Everything to send, per seat.
    pub fn outgoing(&mut self, now_ms: u64) -> Vec<(SeatId, Outgoing)> {
        self.host.outgoing(now_ms)
    }

    /// The next host event, after the server has read it.
    pub fn poll_event(&mut self) -> Option<HostEvent> {
        self.events.pop_front()
    }

    /// The observer-speed hook, when the server has an observer.
    pub fn observer_throttle(&self) -> Option<ObserverThrottle> {
        self.observer.as_ref().map(|o| ObserverThrottle {
            shared: Arc::clone(&o.shared),
        })
    }

    /// Blocks until the observer has stepped every frame handed to it, or is held by its
    /// throttle, or is gone; then takes its hashes as [`Self::tick`] does. Tests and tools call it
    /// to make the observer's progress deterministic; a live server never does.
    pub fn sync_observer(&mut self) {
        if let Some(obs) = self.observer.as_ref() {
            let mut s = lock(&obs.shared);
            while !((s.stepped >= s.sent && s.loads_done >= s.loads_sent) || s.budget == Some(0) || s.dead) {
                s = obs
                    .shared
                    .1
                    .wait(s)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        self.take_hashes();
        self.check_observer();
        self.take_events();
    }

    fn take_hashes(&mut self) {
        let Some(obs) = self.observer.as_mut() else {
            return;
        };
        while let Ok(report) = obs.hashes.try_recv() {
            match report {
                ObserverReport::Hash(tick, hash) => {
                    if obs.recent.len() >= OBSERVER_HASHES {
                        obs.recent.pop_front();
                    }
                    obs.recent.push_back((tick, hash));
                    // Out of the quorum the vote is not counted; the probe's hash ring still
                    // fills. Under a transfer it only counts when it equals the reference.
                    self.host.report_hash(tick, hash);
                }
                ObserverReport::Loaded(tick, Some((hash, replayed))) => {
                    if obs.loading == Some(tick) {
                        obs.loading = None;
                    }
                    obs.resyncs += 1;
                    // Its live hashes from before the load are from the diverged state: the
                    // loaded and replayed ones replace them, here and in the probe's ring, before
                    // the vote can close a checkpoint and start a probe (ADR-10147).
                    obs.recent.retain(|(t, _)| *t < tick);
                    for &(t, h) in std::iter::once(&(tick, hash)).chain(&replayed) {
                        if obs.recent.len() >= OBSERVER_HASHES {
                            obs.recent.pop_front();
                        }
                        obs.recent.push_back((t, h));
                        self.host.report_replayed_hash(t, h);
                    }
                    tracing::info!(tick, "lockstep server: the observer took the majority's state");
                    self.host.revote(tick, hash);
                }
                ObserverReport::Loaded(tick, None) => {
                    if obs.loading == Some(tick) {
                        obs.loading = None;
                    }
                    tracing::warn!(tick, "lockstep server: the observer refused the majority's state");
                    obs.transfer = None;
                    if !obs.diverged {
                        obs.diverged = true;
                        if obs.voting {
                            obs.voting = false;
                            self.host.set_observer_voting(false);
                        }
                    }
                }
            }
        }
    }

    /// Hands the observer the state of the majority seat a [`HostEvent::ResyncHost`] named, the
    /// game's bytes of that seat's state after the frame at `tick` (what the seat sends for
    /// [`crate::lockstep::SeatEvent::ProvideState`]). The observer thread loads it
    /// ([`LockstepObserver::load_state`]), steps the frames after `tick` the host still holds,
    /// and votes again on `tick`: the quorum counts it again once that hash is the reference.
    /// Never waits for the thread.
    ///
    /// Refused with [`LockstepServerError::ObserverNoTransfer`] when no transfer for `tick` is
    /// pending, and with [`LockstepServerError::ObserverStateTooOld`] when the frames after `tick`
    /// are gone (the host orders the transfer again at its deadline).
    pub fn load_observer_state(&mut self, tick: u64, state: Vec<u8>) -> Result<(), LockstepServerError> {
        let next = self.host.tick();
        let Some(obs) = self.observer.as_mut() else {
            return Err(LockstepServerError::ObserverNoTransfer { tick });
        };
        if obs.transfer != Some(tick) || obs.detached || obs.diverged {
            return Err(LockstepServerError::ObserverNoTransfer { tick });
        }
        let replay: Vec<LockstepFrame<G::Input>> = self.host.frames_since(tick + 1).cloned().collect();
        let whole = replay.first().map_or(tick + 1 >= next, |f| f.tick == tick + 1)
            && replay.last().is_none_or(|f| f.tick + 1 == next);
        if !whole {
            return Err(LockstepServerError::ObserverStateTooOld { tick });
        }
        let Some(tx) = obs.frames.as_ref() else {
            return Err(LockstepServerError::ObserverNoTransfer { tick });
        };
        if tx.send(ObserverMsg::Load { tick, state, replay }).is_err() {
            obs.frames = None;
            return Err(LockstepServerError::ObserverNoTransfer { tick });
        }
        lock(&obs.shared).loads_sent += 1;
        obs.transfer = None;
        obs.loading = Some(tick);
        Ok(())
    }

    /// The checkpoint of the state transfer the observer waits for: the tick of the majority
    /// seat's state to hand to [`Self::load_observer_state`]. `None` when none is pending.
    pub fn observer_transfer(&self) -> Option<u64> {
        self.observer.as_ref().and_then(|o| o.transfer)
    }

    fn check_observer(&mut self) {
        let next_build = self.host.tick();
        let max_lag = u64::from(self.settings.observer_max_lag_ticks);
        let rejoin_lag = (max_lag / 2).max(1);
        let backlog = u64::from(self.settings.observer_max_backlog_ticks);
        let Some(obs) = self.observer.as_mut() else {
            return;
        };
        if obs.detached {
            return;
        }
        let (next_tick, dead) = {
            let s = lock(&obs.shared);
            (s.next_tick, s.dead)
        };
        let lag = next_build.saturating_sub(next_tick);
        obs.lag_max = obs.lag_max.max(lag);
        if dead || lag > backlog {
            obs.stop();
            obs.detached = true;
            if obs.voting {
                obs.voting = false;
                obs.dropouts += 1;
                self.host.set_observer_voting(false);
            }
            tracing::warn!(lag, "lockstep server: the observer is detached for the match");
            return;
        }
        if obs.voting && lag > max_lag {
            obs.voting = false;
            obs.dropouts += 1;
            self.host.set_observer_voting(false);
            tracing::info!(
                lag,
                "lockstep server: the observer fell behind and left the quorum"
            );
        } else if !obs.voting && !obs.diverged && lag <= rejoin_lag {
            obs.voting = true;
            obs.rejoins += 1;
            self.host.set_observer_voting(true);
            tracing::info!(lag, "lockstep server: the observer caught up and votes again");
        }
    }

    fn take_events(&mut self) {
        while let Some(event) = self.host.poll_event() {
            match &event {
                HostEvent::Desync(verdict) => {
                    self.verdicts += 1;
                    if self.recent_verdicts.len() >= MAX_VERDICTS {
                        self.recent_verdicts.pop_front();
                    }
                    self.recent_verdicts.push_back(verdict.clone());
                }
                // The observer cannot replay: it answers with the hash it stepped live, which
                // closes the self-check round without waiting for its deadline.
                HostEvent::SelfCheck { tick, .. } => {
                    if let Some(obs) = self.observer.as_mut()
                        && obs.voting
                        && let Some(&(_, hash)) = obs.recent.iter().find(|(t, _)| t == tick)
                    {
                        obs.self_checks += 1;
                        self.host.revote(*tick, hash);
                    }
                }
                // An observer that can load a state waits for the game to deliver the majority
                // seat's (`load_observer_state`); the quorum ignores its votes meanwhile. A newer
                // order replaces an older one.
                HostEvent::ResyncHost {
                    source: Voter::Seat(_),
                    tick,
                    ..
                } if self
                    .observer
                    .as_ref()
                    .is_some_and(|o| o.loads_state && !o.detached && !o.diverged) =>
                {
                    if let Some(obs) = self.observer.as_mut() {
                        obs.transfer = Some(*tick);
                    }
                }
                // An observer that cannot load a state, or the host at fault after its transfers,
                // stops voting for the match.
                HostEvent::ResyncHost { .. } | HostEvent::HostFault { .. } => {
                    if let Some(obs) = self.observer.as_mut()
                        && !obs.diverged
                    {
                        obs.diverged = true;
                        obs.transfer = None;
                        if obs.voting {
                            obs.voting = false;
                            self.host.set_observer_voting(false);
                        }
                    }
                }
                _ => {}
            }
            self.events.push_back(event);
        }
    }

    /// The status rows (`server.lockstep_status`, the health endpoint's `lockstep` object).
    pub fn status(&mut self, now_ms: u64) -> LockstepServerStatus {
        let tick = self.host.tick();
        let observer = self.observer.as_ref().map(|o| {
            let s = lock(&o.shared);
            ObserverStatus {
                attached: !o.detached,
                voting: o.voting,
                next_tick: s.next_tick,
                lag_ticks: tick.saturating_sub(s.next_tick),
                lag_max_ticks: o.lag_max,
                frames_stepped: s.stepped,
                dropouts: o.dropouts,
                rejoins: o.rejoins,
                detached: o.detached,
                diverged: o.diverged,
                resyncs: o.resyncs,
                transfer_tick: o.transfer.or(o.loading),
                self_checks: o.self_checks,
                step_us_p50: percentile(&s.step_us, 500),
                step_us_p99: percentile(&s.step_us, 990),
                step_us_max: s.step_us_max,
                stalls: s.stalls,
            }
        });
        LockstepServerStatus {
            tick,
            seats: self.seats.len() as u32,
            max_seats: self.settings.max_seats,
            frames_built: self.frames_built,
            observer,
            verdicts: self.verdicts,
            recent_verdicts: self.recent_verdicts.iter().cloned().collect(),
            seats_refused: self.seats_refused,
            lockstep: self.host.status(now_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockstep::{LockstepSeat, SeatInput};

    struct Game;
    impl NetGame for Game {
        type Message = Vec<u8>;
        type Input = u8;
        type JoinData = ();
        const GAME_ID: &'static str = "test.lockstep.server";
        const PROTOCOL_VERSION: u32 = 1;
    }

    #[derive(Default)]
    struct Sum(u64);
    impl LockstepObserver<u8> for Sum {
        fn step(&mut self, frame: &LockstepFrame<u8>) {
            for (_, input) in &frame.inputs {
                if let SeatInput::Present(i) | SeatInput::Held(i) = input {
                    self.0 = self.0.wrapping_mul(31).wrapping_add(u64::from(*i));
                }
            }
        }
        fn state_hash(&self, tick: u64) -> u64 {
            self.0 ^ tick
        }
    }

    fn on() -> LockstepServerSettings {
        LockstepServerSettings {
            enabled: true,
            ..LockstepServerSettings::default()
        }
    }

    #[test]
    fn the_section_reads_back_and_keeps_its_defaults() {
        let table: toml::Table = "[server]\nname = \"x\"\n".parse().expect("toml");
        let (s, w) = LockstepServerSettings::from_table(&table).expect("no section");
        assert_eq!(s, LockstepServerSettings::default());
        assert!(!s.enabled && s.observer && w.is_empty());
        let set = LockstepServerSettings {
            enabled: true,
            max_seats: 7,
            input_delay_ticks: 3,
            observer: false,
            checkpoint_ticks: 0,
            observer_max_lag_ticks: 10,
            observer_max_backlog_ticks: 10,
        };
        let table: toml::Table = set.to_toml().parse().expect("toml");
        assert_eq!(LockstepServerSettings::from_table(&table).expect("valid").0, set);
        let bad: toml::Table = "[lockstep]\nmax_seats = 0\nobserver = 2\n".parse().expect("toml");
        let errors = LockstepServerSettings::from_table(&bad).expect_err("bad");
        assert_eq!(errors[0].field, "lockstep.observer", "{errors:?}");
    }

    #[test]
    fn the_match_config_takes_the_delay_and_the_checkpoint_interval() {
        let s = LockstepServerSettings {
            input_delay_ticks: 20,
            checkpoint_ticks: 12,
            ..on()
        };
        let c = s.lockstep_config(60);
        assert_eq!((c.initial_delay_ticks, c.hash_every), (20, 12));
        assert!(c.max_delay_ticks >= 20);
    }

    #[test]
    fn bad_settings_and_a_missing_observer_are_refused_with_codes() {
        let bad = LockstepServerSettings { max_seats: 0, ..on() };
        let e = LockstepServer::<Game>::new(&bad, 60, Some(Box::new(Sum::default()))).expect_err("bad");
        assert_eq!(e.code(), "lockstep_settings_invalid");
        let e = LockstepServer::<Game>::new(&on(), 60, None).expect_err("no sim");
        assert_eq!(e.code(), "lockstep_observer_missing");
        let relay = LockstepServerSettings {
            observer: false,
            ..on()
        };
        let server = LockstepServer::<Game>::new(&relay, 60, Some(Box::new(Sum::default()))).expect("relay");
        assert_eq!(server.host().role(), HostRole::Relay);
        assert!(server.observer_throttle().is_none());
    }

    /// One seat and the observer; the status JSON carries the rows the health endpoint shows.
    #[test]
    fn the_status_json_names_the_rows() {
        let mut server =
            LockstepServer::<Game>::new(&on(), 60, Some(Box::new(Sum::default()))).expect("server");
        server.add_seat(SeatId(0)).expect("seat");
        server.start(0);
        let mut seat = LockstepSeat::<Game>::new(server.config().clone(), SeatId(0));
        let mut sim = Sum::default();
        for now in (0..2_000u64).step_by(8) {
            let _ = seat.submit_input(&3);
            for out in seat.outgoing(now) {
                server
                    .receive(SeatId(0), out.lane, &out.bytes, now)
                    .expect("bytes");
            }
            server.tick(now);
            server.sync_observer();
            for (_, out) in server.outgoing(now) {
                seat.receive(out.lane, &out.bytes, now).expect("frames");
            }
            while let Some(frame) = seat.next_frame() {
                sim.step(&frame);
                seat.report_hash(frame.tick, sim.state_hash(frame.tick));
            }
        }
        let status = server.status(2_000);
        assert!(status.lockstep.checkpoints_agreed >= 3, "{status:?}");
        let json = status.health_json();
        for key in [
            "\"tick\":",
            "\"seats\":1",
            "\"max_seats\":40",
            "\"verdicts\":0",
            "\"last_verdict\":null",
            "\"observer\":{\"voting\":true",
            "\"lag_ticks\":0",
        ] {
            assert!(json.contains(key), "{key} in {json}");
        }
        assert!(json.starts_with('{') && json.ends_with('}'));
        let lines = status.console_lines();
        assert!(
            lines[0].starts_with("lockstep tick=") && lines.len() == 2,
            "{lines:?}"
        );
    }
}
