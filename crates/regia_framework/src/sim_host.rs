// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/present.rs, commit d6ed37677), ADR-8012.

//! Threaded fixed-step host for an external simulation (`docs/specs/25-sim-bridge-and-do27.md` § 3,
//! ADR-8083).
//!
//! A [`FixedStepSim`] is a deterministic simulation that advances one fixed tick at a time. A
//! [`ThreadedSimHost`] moves it onto its own thread, steps it on a fixed clock and publishes the
//! last **two** frames as one pair ([`PublishedFrames`]): the presentation side blends between
//! `prev` and `curr` and never waits for the sim. The host implements
//! [`ExternalSim`], so a game hands it straight to
//! [`SimBridgePlugin`](crate::sim_bridge::SimBridgePlugin).
//!
//! # The publish contract
//!
//! - `prev`, `curr`, both ticks, both timestamps and the publish sequence change together, under
//!   one lock, so a reader never sees a pair from two different publishes (a *torn* pair) and never
//!   sees a frame that is being written (frames are immutable behind an [`Arc`] once published).
//! - While stepping, `curr_tick == prev_tick + 1`. A held (paused) host, and the boot pair, publish
//!   `prev == curr` with equal timestamps, so a blend does not drift while the sim is still.
//! - The host recycles the frame buffer of the dropped `prev` when no reader holds it, so a
//!   steady sim allocates no frame per tick.
//!
//! # Clocks
//!
//! [`HostPacing::Realtime`] steps on the wall clock. When the thread falls behind it runs up to
//! `catch_up` ticks back to back and then drops the rest of the debt, so a stall (a load hitch, a
//! debugger break) never bursts the sim forward by seconds. [`HostPacing::Manual`] steps only the
//! ticks granted with [`SimHostHandle::grant_ticks`]: tests, render-outs and agents use it to step
//! deterministically.
//!
//! [`HostPacing::Lockstep`] steps a [`LockstepSim`] on exact per-tick input frames instead (below).
//!
//! # Lockstep pacing
//!
//! [`ThreadedSimHost::start_lockstep`] returns a [`LockstepFeed`] beside the host. A lockstep session
//! (N224's seat, a replay, a test) pushes one agreed frame per tick with
//! [`LockstepFeed::push_frame`]; the host steps the frame for `tick + 1` only, each exactly once and
//! in order, through [`LockstepSim::step_inputs`]. Nothing is merged. A gap stalls the host (counted
//! in [`SimHostStatus::frame_stalls`]) until the missing frame arrives; a duplicate or an old frame is
//! refused with [`SimHostError::FrameOutOfOrder`]. The clock decides *when* a wake may step (one tick
//! per wake, or one per grant with `tick: None`); [`SimHostHandle::catch_up`] lets each wake step up
//! to a budget of queued frames back to back, so a late joiner or a resynced seat reaches the live
//! tick faster than real time without a stall of unbounded length (spec 120 § 7.4).
//!
//! # Snapshots and restore
//!
//! A host started with [`ThreadedSimHost::start_with_snapshots`] or
//! [`ThreadedSimHost::start_lockstep`] (the sim implements
//! [`DetState`]) takes framed snapshots
//! ([`regia_detsim::state::snapshot`]) and restores them ([`regia_detsim::state::restore_with`]) **on
//! its own thread**, between two steps: [`SimHostHandle::request_snapshot`] answers when the sim
//! reaches a tick, and [`SimHostHandle::restore`] loads the bytes, replaces the sim and republishes a
//! still boot pair at the restored tick. No other thread ever touches the sim.
//!
//! # The floating-point environment
//!
//! The host thread calls [`regia_detsim::fpu::ensure_default_fp_env`] before anything else, so a sim
//! never steps under flush-to-zero or another rounding mode it inherited from the thread that started
//! it ([`SimHostStatus::fp_env_reset`]). It checks again every
//! [`SimHostSettings::fp_check_every`] steps (every step in debug builds) in case foreign code on
//! the sim thread changed it, puts it back and counts it ([`SimHostStatus::fp_env_changed`]).
//!
//! # Tick profile
//!
//! [`FixedStepSim::tick_profile`] publishes a [`regia_detsim::profile::ProfileReport`] after each
//! step. The host stores it for [`SimHostHandle::tick_profile`] and does not fold it into the
//! hashed sim state. [`DetTickProfile`] is the same report as a resource for a game that does
//! not run this host. The agent method is `sim.tick_profile` (ADR-8498).
//!
//! # Input and events
//!
//! [`ExternalSim::submit_input`] never blocks on the
//! sim: it merges into an input bus the thread takes once per tick (latest axes, every edge in
//! order). A held host drops the input it is offered, and so does a lockstep host (its frames carry
//! the input). Events the sim emits are queued for
//! [`ExternalSim::drain_events`]; the queue is
//! bounded ([`SimHostSettings::event_cap`]) and drops its oldest entries, counted in
//! [`SimHostStatus::events_dropped`].
//!
//! # Desync rows of an external sim
//!
//! A sim that also implements [`SystemFold`] (the per-system desync fold) starts with
//! [`ThreadedSimHost::start_with_desync`]: the host thread records the sim's vector after every
//! step, before it publishes the frame, into a [`DesyncFeed`]. The host's [`SimHostHandle`] carries
//! the reader ([`SimHostHandle::desync`]), so `sim.desync_rows` answers for the sim when the app has no
//! ECS desync hash (ADR-8547). [`ThreadedSimHost::start_with_snapshots_and_desync`] and
//! [`ThreadedSimHost::start_lockstep_with_desync`] add the rows to a host that restores: a restore to
//! tick `T` clears the ring and records the restored state under `T`, so no row of the old timeline
//! is compared (ADR-8621).
//!
//! Adapted from Direct Order 27's presentation clock (`shell-regia/game/src/present.rs`, SHA
//! d6ed37677, regia-absorb-1; see `docs/ported-sources.toml`), with its game hooks removed and its
//! caps turned into [`SimHostSettings`].

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_detsim::fpu;
use regia_detsim::hash::SystemFold;
use regia_detsim::state::{self, DetReadLimits, DetState, DetStateError, SnapshotHeader};

use crate::desync_hash::{DesyncFeed, SimHostDesync};
use crate::sim_blend::SnapshotPair;
use crate::sim_bridge::{BridgeInput, ExternalSim, SimSnapshot, SnapshotTick};

/// Tick-profiler report stored by the sim host and returned by `sim.tick_profile` (ADR-8498).
pub use regia_detsim::profile::ProfileReport;

mod det_input;
pub use det_input::{DetInputFrame, DetInputHost, adapter_tag};

use regia_detsim::debug::{DebugRecorder, DebugWhat};

/// One 60 Hz tick (16 666 667 ns), the default fixed step.
pub const DEFAULT_TICK: Duration = Duration::from_nanos(16_666_667);

/// Most input edges the bus keeps between two ticks. More are dropped and counted
/// ([`SimHostStatus::edges_dropped`]); a frame never produces this many.
pub const MAX_PENDING_EDGES: usize = 1024;

/// Largest single grant [`SimHostHandle::grant_ticks`] accepts.
pub const MAX_GRANT: u64 = 1_000_000;

/// Steps between two checks of the host thread's floating-point environment in release builds
/// ([`SimHostSettings::fp_check_every`]; debug builds check before every step).
pub const DEFAULT_FP_CHECK_EVERY: u32 = 600;

/// Frames a lockstep host queues ahead of the tick it stepped last
/// ([`SimHostSettings::frame_queue_cap`]): about 68 s at 60 Hz, enough for a late joiner's backlog.
pub const DEFAULT_FRAME_QUEUE_CAP: usize = 4096;

/// Largest per-wake budget [`SimHostHandle::catch_up`] accepts.
pub const MAX_CATCH_UP: u32 = 10_000;

// ---------------------------------------------------------------------------
// The sim trait
// ---------------------------------------------------------------------------

/// A deterministic simulation that advances one fixed tick at a time.
///
/// The host owns the sim on its own thread: [`step`](FixedStepSim::step) and
/// [`write_frame`](FixedStepSim::write_frame) are only ever called there, one after the other.
pub trait FixedStepSim: Send + 'static {
    /// What one tick publishes: the render-ready state. Immutable once published.
    type Frame: Default + Send + Sync + 'static;

    /// Events the sim emits (audio cues, effects, toasts).
    type Event: Send + Sync + 'static;

    /// Advance one tick with this tick's merged input.
    fn step(&mut self, input: &BridgeInput);

    /// The sim's own tick counter (the number published with each frame).
    fn tick(&self) -> u64;

    /// Write the current state into `out`. `out` may hold an older frame (the host recycles
    /// buffers): overwrite every field.
    fn write_frame(&self, out: &mut Self::Frame);

    /// Move the events emitted since the last call into `out`. The default emits none.
    fn drain_events(&mut self, _out: &mut Vec<Self::Event>) {}

    /// The tick profile for the step just taken. The default publishes none.
    ///
    /// The host stores this after [`step`](FixedStepSim::step) and does not hash it. A sim that
    /// profiles keeps the [`regia_detsim::profile::TickProfiler`] beside its state and returns
    /// [`TickProfiler::report`](regia_detsim::profile::TickProfiler::report) here.
    fn tick_profile(&self) -> Option<ProfileReport> {
        None
    }

    /// Draw the sim's deterministic indexes into `out` (ADR-8562). The default draws nothing
    /// and returns `false`: the sim has no det debug draw.
    ///
    /// A sim that draws returns `true`, also when `what` is empty (the host asks once at start
    /// with an empty set to learn whether the sim draws). While `what` is non-empty the host
    /// calls this after every step; turn the indexes' capture on here
    /// (`set_debug_capture(Some(DebugCapture::default()))`, see
    /// [`crate::det_debug::DetDebugDraw::record`]) and off when `what` is empty. Capture reads
    /// state only and is not hashed.
    fn det_debug(
        &mut self,
        _what: regia_detsim::debug::DebugWhat,
        _out: &mut regia_detsim::debug::DebugRecorder,
    ) -> bool {
        false
    }
}

/// A [`FixedStepSim`] that steps exact per-tick input frames: the lockstep path
/// ([`HostPacing::Lockstep`], [`ThreadedSimHost::start_lockstep`]).
///
/// [`step_inputs`](LockstepSim::step_inputs) is called on the host thread only, once per tick, in
/// tick order, with the frame a [`LockstepFeed`] queued for that tick. `Inputs` is whatever one
/// agreed tick carries: N224's `regia_net::lockstep::LockstepFrame<I>`, the seat-ordered input list of
/// a replay, or a game's own type. [`FixedStepSim::step`] (merged bridge input) is not called by a
/// lockstep host.
pub trait LockstepSim: FixedStepSim {
    /// One tick's agreed inputs for every seat.
    type Inputs: Send + 'static;

    /// Advance exactly one tick, `tick` (always `self.tick() + 1`), with that tick's inputs. After it
    /// returns, [`FixedStepSim::tick`] must report `tick`.
    fn step_inputs(&mut self, tick: u64, inputs: &Self::Inputs);
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// How the host thread decides when to step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPacing {
    /// Step on the wall clock, one tick every `tick`.
    Realtime {
        /// The fixed step (must not be zero).
        tick: Duration,
        /// Ticks the thread may run back to back to catch up after falling behind; the rest of the
        /// debt is dropped. `0` runs one tick per wake and drops any debt at once.
        catch_up: u32,
    },
    /// Step only ticks granted with [`SimHostHandle::grant_ticks`] (tests, render-outs, agents).
    Manual,
    /// Step a [`LockstepSim`] on exact per-tick frames from a [`LockstepFeed`]: a wake steps the
    /// frame for the next tick only when it is queued (one frame, or up to the
    /// [`SimHostHandle::catch_up`] budget), and a gap stalls until it arrives. Needs
    /// [`ThreadedSimHost::start_lockstep`].
    Lockstep {
        /// One wake per `tick` on the wall clock; `None` wakes once per granted tick
        /// ([`SimHostHandle::grant_ticks`]), which tests, replays and agents use to step
        /// deterministically. A zero tick is refused.
        tick: Option<Duration>,
    },
}

/// The host's configuration. [`Default`] is a 60 Hz realtime host with no catch-up.
#[derive(Debug, Clone, PartialEq)]
pub struct SimHostSettings {
    /// When to step.
    pub pacing: HostPacing,
    /// Start held (paused): the boot pair is published and nothing steps until
    /// [`SimHostHandle::set_held`]`(false)`. A lobby preloads a match this way.
    pub start_held: bool,
    /// Step only while the presentation stamped a frame ([`SimHostHandle::stamp_client`]) within
    /// this long, so a load stall cannot run the sim unseen. `None` (the default) never waits.
    /// Realtime pacing only.
    pub client_stale_after: Option<Duration>,
    /// Most undrained events kept; older ones are dropped and counted.
    pub event_cap: usize,
    /// The thread's name (shows in profilers and crash reports).
    pub thread_name: String,
    /// Steps between two checks of the thread's floating-point environment (a changed one is put
    /// back and counted in [`SimHostStatus::fp_env_changed`]). `0` checks only at thread start.
    /// Default: every step in debug builds, [`DEFAULT_FP_CHECK_EVERY`] in release builds.
    pub fp_check_every: u32,
    /// Most frames a lockstep host queues ahead of the tick it stepped last; a frame further ahead
    /// is refused with [`SimHostError::FrameQueueFull`].
    pub frame_queue_cap: usize,
    /// Reader limits for [`SimHostHandle::restore`] (a lobby that sends large states raises them).
    pub restore_limits: DetReadLimits,
}

impl Default for SimHostSettings {
    fn default() -> Self {
        Self {
            pacing: HostPacing::Realtime {
                tick: DEFAULT_TICK,
                catch_up: 0,
            },
            start_held: false,
            client_stale_after: None,
            event_cap: 1024,
            thread_name: "regia-sim".to_string(),
            fp_check_every: if cfg!(debug_assertions) {
                1
            } else {
                DEFAULT_FP_CHECK_EVERY
            },
            frame_queue_cap: DEFAULT_FRAME_QUEUE_CAP,
            restore_limits: DetReadLimits::DEFAULT,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a host call could not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SimHostError {
    /// The settings are unusable (a zero realtime tick).
    InvalidSettings(String),
    /// The operating system refused the thread.
    Spawn(String),
    /// The sim panicked on the host thread; the host stopped.
    SimPanicked,
    /// The host was stopped before the call could complete.
    Stopped,
    /// A grant was made to a realtime host (grants drive [`HostPacing::Manual`] only).
    NotManual,
    /// A grant was zero or above [`MAX_GRANT`].
    BadGrant(u64),
    /// Frames and catch-up need a lockstep host ([`ThreadedSimHost::start_lockstep`]).
    NotLockstep,
    /// A frame for a tick the host already stepped or already queued (a duplicate or an old
    /// frame). `next` is the lowest tick the host still accepts. Agent code `frame_out_of_order`.
    FrameOutOfOrder {
        /// The refused frame's tick.
        tick: u64,
        /// The lowest tick a frame may carry now.
        next: u64,
    },
    /// A frame more than [`SimHostSettings::frame_queue_cap`] ticks ahead of the last stepped tick.
    FrameQueueFull {
        /// The refused frame's tick.
        tick: u64,
        /// The queue cap.
        cap: usize,
    },
    /// A catch-up budget was zero or above [`MAX_CATCH_UP`].
    BadCatchUp(u32),
    /// The host was started without snapshots: the sim does not implement `DetState`, or the host
    /// was started with [`ThreadedSimHost::start`].
    NoSnapshots,
    /// A snapshot was asked for a tick the sim has already passed.
    SnapshotTickPassed {
        /// The tick asked for.
        tick: u64,
        /// The sim's tick when the request was served.
        curr: u64,
    },
    /// Saving the sim failed (a length over `u32`, or a serde value the codec refuses).
    SnapshotFailed(DetStateError),
    /// The bytes were refused (bad frame, tag, version, hash, or a tick that disagrees with the
    /// loaded sim); the sim is unchanged.
    RestoreFailed(DetStateError),
    /// The host thread did not answer a command within the caller's timeout.
    Busy,
}

impl fmt::Display for SimHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSettings(why) => write!(f, "invalid sim host settings: {why}"),
            Self::Spawn(why) => write!(f, "sim host thread: {why}"),
            Self::SimPanicked => f.write_str("the sim panicked on the host thread"),
            Self::Stopped => f.write_str("the sim host is stopped"),
            Self::NotManual => f.write_str("the sim host steps on the wall clock; grants need manual pacing"),
            Self::BadGrant(n) => write!(f, "a grant must be 1..={MAX_GRANT} ticks, not {n}"),
            Self::NotLockstep => f.write_str("frames and catch-up need a lockstep sim host"),
            Self::FrameOutOfOrder { tick, next } => {
                write!(
                    f,
                    "frame for tick {tick} is a duplicate or old; the host accepts {next} and later"
                )
            }
            Self::FrameQueueFull { tick, cap } => {
                write!(
                    f,
                    "frame for tick {tick} is more than {cap} ticks ahead of the last stepped tick"
                )
            }
            Self::BadCatchUp(n) => write!(f, "a catch-up budget must be 1..={MAX_CATCH_UP} ticks, not {n}"),
            Self::NoSnapshots => f.write_str("this sim host was started without snapshots"),
            Self::SnapshotTickPassed { tick, curr } => {
                write!(
                    f,
                    "a snapshot at tick {tick} was asked after the sim reached tick {curr}"
                )
            }
            Self::SnapshotFailed(err) => write!(f, "snapshot failed: {err}"),
            Self::RestoreFailed(err) => write!(f, "restore refused: {err}"),
            Self::Busy => f.write_str("the sim host thread did not answer in time"),
        }
    }
}

impl std::error::Error for SimHostError {}

// ---------------------------------------------------------------------------
// The published pair
// ---------------------------------------------------------------------------

/// The last two published frames, read as one consistent pair.
pub struct PublishedFrames<F> {
    /// The frame before `curr` (equal to `curr` at boot and while held).
    pub prev: Arc<F>,
    /// The newest frame.
    pub curr: Arc<F>,
    /// The sim tick of `prev`.
    pub prev_tick: u64,
    /// The sim tick of `curr`.
    pub curr_tick: u64,
    /// When `prev` was published.
    pub prev_at: Instant,
    /// When `curr` was published.
    pub curr_at: Instant,
    /// How many times the pair changed since boot (0 is the boot pair).
    pub sequence: u64,
}

impl<F> Clone for PublishedFrames<F> {
    fn clone(&self) -> Self {
        Self {
            prev: Arc::clone(&self.prev),
            curr: Arc::clone(&self.curr),
            prev_tick: self.prev_tick,
            curr_tick: self.curr_tick,
            prev_at: self.prev_at,
            curr_at: self.curr_at,
            sequence: self.sequence,
        }
    }
}

impl<F> fmt::Debug for PublishedFrames<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublishedFrames")
            .field("prev_tick", &self.prev_tick)
            .field("curr_tick", &self.curr_tick)
            .field("sequence", &self.sequence)
            .finish_non_exhaustive()
    }
}

impl<F> PublishedFrames<F> {
    /// Whether the pair is still (boot or held): `prev` and `curr` are the same frame.
    pub fn is_still(&self) -> bool {
        Arc::ptr_eq(&self.prev, &self.curr)
    }

    /// The blend weight from `prev` (0) to `curr` (1) for a frame presented at `now`, drawing
    /// `delay` behind the newest publish (one tick is the usual choice). See [`blend_alpha`].
    pub fn alpha(&self, now: Instant, delay: Duration) -> f32 {
        blend_alpha(self.prev_at, self.curr_at, now, delay)
    }
}

/// The blend weight between two published frames for a frame presented at `now`.
///
/// The presentation draws `delay` behind the clock: the render time is `now - delay`, placed on the
/// `prev_at..curr_at` span and clamped to `0..=1`. A still pair (equal or reversed timestamps)
/// returns `1.0` (draw `curr`).
pub fn blend_alpha(prev_at: Instant, curr_at: Instant, now: Instant, delay: Duration) -> f32 {
    let Some(span) = curr_at.checked_duration_since(prev_at) else {
        return 1.0;
    };
    if span.is_zero() {
        return 1.0;
    }
    let render_at = now.checked_sub(delay).unwrap_or(prev_at);
    let along = render_at.saturating_duration_since(prev_at);
    (along.as_secs_f32() / span.as_secs_f32()).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// A point-in-time view of a host's counters (the `sim.host_status` agent method).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SimHostStatus {
    /// The host thread is running (not stopped, not panicked).
    pub running: bool,
    /// The sim panicked on the host thread.
    pub panicked: bool,
    /// The host is held (paused).
    pub held: bool,
    /// The host steps on granted ticks ([`HostPacing::Manual`]).
    pub manual: bool,
    /// The realtime tick in nanoseconds (0 for manual pacing).
    pub tick_ns: u64,
    /// Ticks stepped since boot.
    pub ticks: u64,
    /// Pair changes since boot ([`PublishedFrames::sequence`]).
    pub publishes: u64,
    /// The sim tick of the newest published frame.
    pub curr_tick: u64,
    /// Manual ticks granted and not stepped yet.
    pub granted: u64,
    /// Times the realtime clock dropped debt it could not catch up.
    pub debt_drops: u64,
    /// Whole ticks of wall time skipped by those drops.
    pub ticks_dropped: u64,
    /// Wakes that did not step because the presentation's frame stamp was stale.
    pub client_waits: u64,
    /// Events dropped because the queue was full.
    pub events_dropped: u64,
    /// Input edges dropped (a full bus, offered while held, or offered to a lockstep host).
    pub edges_dropped: u64,
    /// The host steps exact frames ([`HostPacing::Lockstep`]).
    pub lockstep: bool,
    /// The target has a floating-point environment guard (x86-64, aarch64).
    pub fp_env_supported: bool,
    /// The host thread started with a non-default floating-point environment (flush-to-zero,
    /// another rounding mode, traps) and the guard put it back before the first step.
    pub fp_env_reset: bool,
    /// The raw control register (MXCSR or FPCR) the thread started with; `0` when unsupported.
    pub fp_env_at_start: u64,
    /// Periodic environment checks run since boot ([`SimHostSettings::fp_check_every`]).
    pub fp_checks: u64,
    /// Periodic checks that found the environment changed (and put it back).
    pub fp_env_changed: u64,
    /// Lockstep frames queued and not stepped yet.
    pub frames_queued: u64,
    /// Lockstep wakes that found no frame for the next tick and waited.
    pub frame_stalls: u64,
    /// Lockstep frames refused (out of order, duplicate, or past the queue cap).
    pub frames_refused: u64,
    /// The active catch-up budget (ticks per wake); `0` when not catching up.
    pub catch_up_budget: u32,
    /// Ticks stepped as extra catch-up steps (beyond one per wake).
    pub catch_up_ticks: u64,
    /// Snapshots taken on the host thread.
    pub snapshots: u64,
    /// Restores applied on the host thread.
    pub restores: u64,
    /// Wall time of the newest sim step, in nanoseconds (the step call only, not the publish).
    pub last_step_ns: u64,
    /// Longest sim step since boot, in nanoseconds.
    pub max_step_ns: u64,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// The type-erased half: flags, the step clock's condition and counters.
#[derive(Debug, Default)]
struct Control {
    gate: Mutex<Gate>,
    wake: Condvar,
    client_beat_ms: AtomicU64,
    origin: OnceInstant,
    manual: bool,
    tick_ns: u64,
    running: AtomicBool,
    panicked: AtomicBool,
    ticks: AtomicU64,
    publishes: AtomicU64,
    curr_tick: AtomicU64,
    debt_drops: AtomicU64,
    ticks_dropped: AtomicU64,
    client_waits: AtomicU64,
    events_dropped: AtomicU64,
    edges_dropped: AtomicU64,
    /// Report from the step that published the current tick. `None` when the sim does not profile.
    tick_profile: Mutex<Option<ProfileReport>>,
    lockstep: bool,
    snapshots_enabled: bool,
    fp_supported: AtomicBool,
    fp_reset: AtomicBool,
    fp_at_start: AtomicU64,
    fp_checks: AtomicU64,
    fp_changed: AtomicU64,
    frames_queued: AtomicU64,
    frame_stalls: AtomicU64,
    frames_refused: AtomicU64,
    catch_up_ticks: AtomicU64,
    snapshots: AtomicU64,
    restores: AtomicU64,
    last_step_ns: AtomicU64,
    max_step_ns: AtomicU64,
    /// Bumped (under its lock) whenever a counter a waiter may watch changes.
    changes: Mutex<u64>,
    changed: Condvar,
    /// Mirrors `Gate::catch_up` for lock-free status reads.
    catch_up_budget: AtomicU32,
    /// Tests: the host thread raises the hold itself right after it takes the next grant, which
    /// places a hold exactly between taking a grant and stepping it.
    #[cfg(test)]
    hold_after_grant: AtomicBool,
    /// The det debug switch (`DebugWhat` bits) the agent or [`crate::det_debug`] set (ADR-8562).
    det_debug_what: AtomicU32,
    /// The sim answered `true` to the start-up `det_debug` probe.
    det_debug_supported: AtomicBool,
    /// The newest det debug frame and the tick it was drawn at.
    det_debug_frame: Mutex<Option<(u64, DebugRecorder)>>,
}

#[derive(Debug, Default)]
struct Gate {
    stop: bool,
    held: bool,
    granted: u64,
    /// Snapshot and restore requests the host thread runs between two steps.
    commands: VecDeque<Command>,
    /// Lockstep catch-up budget (ticks per wake); `0` steps one tick per wake.
    catch_up: u32,
}

/// A request the host thread runs between two steps (never from another thread into the sim).
#[derive(Debug)]
enum Command {
    Snapshot {
        at: Option<u64>,
        reply: mpsc::Sender<Result<HostSnapshot, SimHostError>>,
    },
    Restore {
        bytes: Vec<u8>,
        reply: mpsc::Sender<Result<u64, SimHostError>>,
    },
}

/// A monotonic origin for the presentation stamp (stored as milliseconds, never 0).
#[derive(Debug)]
struct OnceInstant(Instant);

impl Default for OnceInstant {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl Control {
    fn gate(&self) -> MutexGuard<'_, Gate> {
        self.gate.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.0.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    /// Wake every [`SimHostHandle::wait_status`] caller.
    fn bump(&self) {
        let mut changes = lock(&self.changes);
        *changes = changes.wrapping_add(1);
        drop(changes);
        self.changed.notify_all();
    }

    /// The error for a host that cannot take a call any more, if it cannot.
    fn not_running(&self) -> Option<SimHostError> {
        if self.panicked.load(Ordering::Acquire) {
            Some(SimHostError::SimPanicked)
        } else if !self.running.load(Ordering::Acquire) {
            Some(SimHostError::Stopped)
        } else {
            None
        }
    }

    /// Queue a command for the host thread and wake it.
    fn send(&self, command: Command) -> Result<(), SimHostError> {
        if let Some(err) = self.not_running() {
            return Err(err);
        }
        let mut gate = self.gate();
        // Checked under the gate: the thread clears `running` before it takes the gate to drop
        // the commands nobody will run, so a command queued here is either run or dropped.
        if gate.stop || !self.running.load(Ordering::Acquire) {
            return Err(SimHostError::Stopped);
        }
        gate.commands.push_back(command);
        drop(gate);
        self.wake.notify_all();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Snapshots and command replies
// ---------------------------------------------------------------------------

/// A framed snapshot the host thread took ([`SimHostHandle::request_snapshot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSnapshot {
    /// The tick boundary the snapshot was taken at (that many ticks had run).
    pub tick: u64,
    /// The frame header: tag, state version, body length and the body hash.
    pub header: SnapshotHeader,
    /// The framed bytes ([`regia_detsim::state::snapshot`]); [`SimHostHandle::restore`] loads them.
    pub bytes: Vec<u8>,
}

/// The answer to a command the host thread runs later ([`SimHostHandle::request_snapshot`],
/// [`SimHostHandle::restore`]).
#[derive(Debug)]
pub struct HostReply<T> {
    rx: mpsc::Receiver<Result<T, SimHostError>>,
    control: Arc<Control>,
}

impl<T> HostReply<T> {
    /// Block until the host thread answers. A host that stops (or panics) first answers
    /// [`SimHostError::Stopped`] (or [`SimHostError::SimPanicked`]).
    pub fn wait(self) -> Result<T, SimHostError> {
        match self.rx.recv() {
            Ok(answer) => answer,
            Err(mpsc::RecvError) => Err(self.gone()),
        }
    }

    /// Block at most `timeout`; [`SimHostError::Busy`] when the host thread has not answered by
    /// then (the reply stays pending; call again to keep waiting).
    pub fn wait_timeout(&self, timeout: Duration) -> Result<T, SimHostError> {
        match self.rx.recv_timeout(timeout) {
            Ok(answer) => answer,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(SimHostError::Busy),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.gone()),
        }
    }

    /// The answer if the host thread has given it; `None` while it is pending.
    pub fn try_wait(&self) -> Option<Result<T, SimHostError>> {
        match self.rx.try_recv() {
            Ok(answer) => Some(answer),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(self.gone())),
        }
    }

    fn gone(&self) -> SimHostError {
        self.control.not_running().unwrap_or(SimHostError::Stopped)
    }
}

struct Publish<F> {
    slot: Mutex<PublishedFrames<F>>,
    published: Condvar,
}

struct Bus {
    input: BridgeInput,
}

struct Shared<F, E> {
    control: Arc<Control>,
    desync: Option<SimHostDesync>,
    publish: Publish<F>,
    bus: Mutex<Bus>,
    events: Mutex<VecDeque<E>>,
    event_cap: usize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Handle (type-erased control, a resource agents read)
// ---------------------------------------------------------------------------

/// A cheap, cloneable, type-erased handle to a running host: hold, grant ticks, stamp the
/// presentation, read and wait on the counters, ask for snapshots and restores, and start a
/// catch-up. Insert it as a resource (see [`ThreadedSimHost::handle`]) for the `sim.host_status`,
/// `sim.host_hold`, `sim.host_step`, `sim.host_snapshot` and `sim.host_restore` agent methods, and
/// for the suspend hold (`sim_bridge::hold_sim_host_on_suspend`).
#[derive(Resource, Clone, Debug)]
pub struct SimHostHandle {
    control: Arc<Control>,
    desync: Option<SimHostDesync>,
}

impl SimHostHandle {
    /// Hold (pause) or release the host. A held host keeps its state, publishes a still pair and
    /// drops the input it is offered. Returns whether it was held before.
    pub fn set_held(&self, held: bool) -> bool {
        let mut gate = self.control.gate();
        let was = gate.held;
        gate.held = held;
        drop(gate);
        self.control.wake.notify_all();
        self.control.bump();
        was
    }

    /// Whether the host is held.
    pub fn held(&self) -> bool {
        self.control.gate().held
    }

    /// The sim's desync rows, for a host started with [`ThreadedSimHost::start_with_desync`].
    pub fn desync(&self) -> Option<&SimHostDesync> {
        self.desync.as_ref()
    }

    /// Grant `ticks` more steps to a [`HostPacing::Manual`] host. Returns the ticks granted and
    /// not stepped yet, this grant included.
    pub fn grant_ticks(&self, ticks: u64) -> Result<u64, SimHostError> {
        if !self.control.manual {
            return Err(SimHostError::NotManual);
        }
        if ticks == 0 || ticks > MAX_GRANT {
            return Err(SimHostError::BadGrant(ticks));
        }
        if self.control.panicked.load(Ordering::Acquire) {
            return Err(SimHostError::SimPanicked);
        }
        let mut gate = self.control.gate();
        if gate.stop || !self.control.running.load(Ordering::Acquire) {
            return Err(SimHostError::Stopped);
        }
        gate.granted = gate.granted.saturating_add(ticks);
        let pending = gate.granted;
        drop(gate);
        self.control.wake.notify_all();
        self.control.bump();
        Ok(pending)
    }

    /// The presentation drew a frame. With [`SimHostSettings::client_stale_after`] set, the host
    /// steps only while this stamp is fresh.
    pub fn stamp_client(&self) {
        self.control
            .client_beat_ms
            .store(self.control.now_ms(), Ordering::Release);
    }

    /// Take a framed snapshot of the sim when it reaches `tick` (a tick boundary: right after that
    /// tick's step, before the next one; at once when the sim is at `tick` now). The host thread
    /// takes it; the sim keeps stepping. A tick the sim has passed answers
    /// [`SimHostError::SnapshotTickPassed`].
    pub fn request_snapshot(&self, tick: u64) -> Result<HostReply<HostSnapshot>, SimHostError> {
        self.snapshot_command(Some(tick))
    }

    /// Take a framed snapshot at the next point the host thread is between two steps.
    pub fn request_snapshot_now(&self) -> Result<HostReply<HostSnapshot>, SimHostError> {
        self.snapshot_command(None)
    }

    fn snapshot_command(&self, at: Option<u64>) -> Result<HostReply<HostSnapshot>, SimHostError> {
        if !self.control.snapshots_enabled {
            return Err(SimHostError::NoSnapshots);
        }
        let (reply, rx) = mpsc::channel();
        self.control.send(Command::Snapshot { at, reply })?;
        Ok(HostReply {
            rx,
            control: Arc::clone(&self.control),
        })
    }

    /// Load a framed snapshot on the host thread (through [`regia_detsim::state::restore_with`], so
    /// the sim resumes bit for bit), replace the sim with it and republish a still boot pair at the
    /// restored tick. Queued lockstep frames for that tick and earlier are dropped; later ones stay.
    /// Answers the restored tick. Refused bytes leave the sim unchanged
    /// ([`SimHostError::RestoreFailed`]).
    pub fn restore(&self, bytes: Vec<u8>) -> Result<HostReply<u64>, SimHostError> {
        if !self.control.snapshots_enabled {
            return Err(SimHostError::NoSnapshots);
        }
        let (reply, rx) = mpsc::channel();
        self.control.send(Command::Restore { bytes, reply })?;
        Ok(HostReply {
            rx,
            control: Arc::clone(&self.control),
        })
    }

    /// Let each wake of a lockstep host step up to `max_ticks_per_frame` queued frames back to back
    /// (faster than real time, bounded per wake) until it reaches a tick with no queued frame; then
    /// the host returns to one tick per wake on its own.
    pub fn catch_up(&self, max_ticks_per_frame: u32) -> Result<(), SimHostError> {
        if !self.control.lockstep {
            return Err(SimHostError::NotLockstep);
        }
        if max_ticks_per_frame == 0 || max_ticks_per_frame > MAX_CATCH_UP {
            return Err(SimHostError::BadCatchUp(max_ticks_per_frame));
        }
        if let Some(err) = self.control.not_running() {
            return Err(err);
        }
        let mut gate = self.control.gate();
        gate.catch_up = max_ticks_per_frame;
        self.control
            .catch_up_budget
            .store(max_ticks_per_frame, Ordering::Release);
        drop(gate);
        self.control.wake.notify_all();
        Ok(())
    }

    /// Whether the host takes snapshots and restores ([`ThreadedSimHost::start_with_snapshots`],
    /// [`ThreadedSimHost::start_lockstep`]).
    pub fn snapshots_enabled(&self) -> bool {
        self.control.snapshots_enabled
    }

    /// Block until `until` holds for the host's status, and return that status. Every counter
    /// change wakes the check (steps, stalls, refusals, snapshots, restores, holds). A host that
    /// stops first answers [`SimHostError::Stopped`] (or [`SimHostError::SimPanicked`]) unless
    /// `until` holds for its final status.
    pub fn wait_status(
        &self,
        mut until: impl FnMut(&SimHostStatus) -> bool,
    ) -> Result<SimHostStatus, SimHostError> {
        let control = &self.control;
        loop {
            let seen = *lock(&control.changes);
            let status = self.status();
            if until(&status) {
                return Ok(status);
            }
            if let Some(err) = control.not_running() {
                return Err(err);
            }
            let mut changes = lock(&control.changes);
            while *changes == seen {
                changes = control
                    .changed
                    .wait(changes)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }

    /// The host's counters now.
    pub fn status(&self) -> SimHostStatus {
        let c = &self.control;
        let gate = c.gate();
        let (held, granted) = (gate.held, gate.granted);
        drop(gate);
        SimHostStatus {
            running: c.running.load(Ordering::Acquire),
            panicked: c.panicked.load(Ordering::Acquire),
            held,
            manual: c.manual,
            tick_ns: c.tick_ns,
            ticks: c.ticks.load(Ordering::Acquire),
            publishes: c.publishes.load(Ordering::Acquire),
            curr_tick: c.curr_tick.load(Ordering::Acquire),
            granted,
            debt_drops: c.debt_drops.load(Ordering::Relaxed),
            ticks_dropped: c.ticks_dropped.load(Ordering::Relaxed),
            client_waits: c.client_waits.load(Ordering::Relaxed),
            events_dropped: c.events_dropped.load(Ordering::Relaxed),
            edges_dropped: c.edges_dropped.load(Ordering::Relaxed),
            lockstep: c.lockstep,
            fp_env_supported: c.fp_supported.load(Ordering::Acquire),
            fp_env_reset: c.fp_reset.load(Ordering::Acquire),
            fp_env_at_start: c.fp_at_start.load(Ordering::Acquire),
            fp_checks: c.fp_checks.load(Ordering::Acquire),
            fp_env_changed: c.fp_changed.load(Ordering::Acquire),
            frames_queued: c.frames_queued.load(Ordering::Acquire),
            frame_stalls: c.frame_stalls.load(Ordering::Acquire),
            frames_refused: c.frames_refused.load(Ordering::Acquire),
            catch_up_budget: c.catch_up_budget.load(Ordering::Acquire),
            catch_up_ticks: c.catch_up_ticks.load(Ordering::Acquire),
            snapshots: c.snapshots.load(Ordering::Acquire),
            restores: c.restores.load(Ordering::Acquire),
            last_step_ns: c.last_step_ns.load(Ordering::Relaxed),
            max_step_ns: c.max_step_ns.load(Ordering::Relaxed),
        }
    }

    /// The tick profile stored with the newest step, when the sim publishes one.
    pub fn tick_profile(&self) -> Option<ProfileReport> {
        lock(&self.control.tick_profile).clone()
    }

    /// Whether the sim draws its deterministic indexes ([`FixedStepSim::det_debug`], ADR-8562).
    pub fn det_debug_supported(&self) -> bool {
        self.control.det_debug_supported.load(Ordering::Relaxed)
    }

    /// What the host asks the sim to draw after each step.
    pub fn det_debug(&self) -> DebugWhat {
        DebugWhat::from_bits(self.control.det_debug_what.load(Ordering::Relaxed))
    }

    /// Set what the sim draws after each step (empty turns drawing off). Returns the previous set.
    pub fn set_det_debug(&self, what: DebugWhat) -> DebugWhat {
        DebugWhat::from_bits(self.control.det_debug_what.swap(what.bits(), Ordering::Relaxed))
    }

    /// A copy of the newest det debug frame and its tick; `None` while drawing is off or before
    /// the first step that drew.
    pub fn det_debug_frame(&self) -> Option<(u64, DebugRecorder)> {
        lock(&self.control.det_debug_frame).clone()
    }
}

/// A tick profile for an ECS game that does not run [`ThreadedSimHost`].
///
/// `sim.tick_profile` reads this when the host handle has no report. The report is not
/// simulation state and is not part of a desync hash.
#[derive(Resource, Clone, Debug)]
pub struct DetTickProfile {
    /// The latest report.
    pub report: ProfileReport,
}

// ---------------------------------------------------------------------------
// Lockstep frames
// ---------------------------------------------------------------------------

/// Frames a [`LockstepFeed`] queued, keyed by tick, and the last tick the host stepped.
struct FrameSlots<I> {
    applied: u64,
    frames: BTreeMap<u64, I>,
}

struct FrameQueue<I> {
    slots: Mutex<FrameSlots<I>>,
    control: Arc<Control>,
    cap: usize,
}

impl<I> FrameQueue<I> {
    /// Remove the frame for `tick` if it is queued, and mark `tick` stepped.
    fn take(&self, tick: u64) -> Option<I> {
        let mut slots = lock(&self.slots);
        let inputs = slots.frames.remove(&tick)?;
        slots.applied = tick;
        self.control
            .frames_queued
            .store(slots.frames.len() as u64, Ordering::Release);
        Some(inputs)
    }

    fn has(&self, tick: u64) -> bool {
        lock(&self.slots).frames.contains_key(&tick)
    }

    /// After a restore at `tick`: frames for `tick` and earlier are stale; later ones stay.
    fn rebase(&self, tick: u64) {
        let mut slots = lock(&self.slots);
        slots.applied = tick;
        slots.frames = slots.frames.split_off(&(tick.saturating_add(1)));
        self.control
            .frames_queued
            .store(slots.frames.len() as u64, Ordering::Release);
    }
}

/// The host thread's view of a frame queue for one sim type.
trait FramePort<S>: Send + Sync {
    /// Step the frame for `sim.tick() + 1`; `false` (and no step) when it is not queued.
    fn step_next(&self, sim: &mut S) -> bool;
    /// Whether the frame for `tick` is queued.
    fn has(&self, tick: u64) -> bool;
    /// Drop frames up to `tick` after a restore.
    fn rebase(&self, tick: u64);
}

struct Port<S: LockstepSim>(Arc<FrameQueue<S::Inputs>>);

impl<S: LockstepSim> FramePort<S> for Port<S> {
    fn step_next(&self, sim: &mut S) -> bool {
        let tick = sim.tick().wrapping_add(1);
        let Some(inputs) = self.0.take(tick) else {
            return false;
        };
        sim.step_inputs(tick, &inputs);
        true
    }

    fn has(&self, tick: u64) -> bool {
        self.0.has(tick)
    }

    fn rebase(&self, tick: u64) {
        self.0.rebase(tick);
    }
}

/// Feeds exact per-tick frames to a lockstep host ([`ThreadedSimHost::start_lockstep`]). Cheap to
/// clone; any thread may push (a net seat's thread, a replay reader, a test).
pub struct LockstepFeed<I> {
    queue: Arc<FrameQueue<I>>,
}

impl<I> Clone for LockstepFeed<I> {
    fn clone(&self) -> Self {
        Self {
            queue: Arc::clone(&self.queue),
        }
    }
}

impl<I> fmt::Debug for LockstepFeed<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let slots = lock(&self.queue.slots);
        f.debug_struct("LockstepFeed")
            .field("applied", &slots.applied)
            .field("queued", &slots.frames.len())
            .finish_non_exhaustive()
    }
}

impl<I: Send + 'static> LockstepFeed<I> {
    /// Queue the agreed frame for `tick`. The host steps it once the sim reaches `tick - 1`, in
    /// order, exactly once. Frames may arrive out of order (a gap stalls the host until it is
    /// filled). A frame for a tick the host stepped already, or already queued, is refused with
    /// [`SimHostError::FrameOutOfOrder`]; one more than [`SimHostSettings::frame_queue_cap`] ticks
    /// ahead with [`SimHostError::FrameQueueFull`]. Answers the number of frames queued.
    pub fn push_frame(&self, tick: u64, inputs: I) -> Result<usize, SimHostError> {
        let control = &self.queue.control;
        if let Some(err) = control.not_running() {
            return Err(err);
        }
        let mut slots = lock(&self.queue.slots);
        let refused = if tick <= slots.applied || slots.frames.contains_key(&tick) {
            Some(SimHostError::FrameOutOfOrder {
                tick,
                next: slots.applied.saturating_add(1),
            })
        } else if tick - slots.applied > self.queue.cap as u64 {
            Some(SimHostError::FrameQueueFull {
                tick,
                cap: self.queue.cap,
            })
        } else {
            None
        };
        if let Some(err) = refused {
            drop(slots);
            control.frames_refused.fetch_add(1, Ordering::AcqRel);
            control.bump();
            return Err(err);
        }
        slots.frames.insert(tick, inputs);
        let queued = slots.frames.len();
        control.frames_queued.store(queued as u64, Ordering::Release);
        drop(slots);
        // Take the gate so a host between its frame check and its wait cannot miss this wake.
        drop(control.gate());
        control.wake.notify_all();
        control.bump();
        Ok(queued)
    }

    /// Frames queued and not stepped yet.
    pub fn queued(&self) -> usize {
        lock(&self.queue.slots).frames.len()
    }

    /// The lowest tick a frame may carry now (one past the last stepped tick).
    pub fn next_tick(&self) -> u64 {
        lock(&self.queue.slots).applied.saturating_add(1)
    }
}

// ---------------------------------------------------------------------------
// What the host thread can do beyond stepping
// ---------------------------------------------------------------------------

type SaveFn<S> = fn(&S) -> Result<Vec<u8>, DetStateError>;
type LoadFn<S> = fn(&[u8], DetReadLimits) -> Result<(u64, S), DetStateError>;

struct Caps<S> {
    save: Option<SaveFn<S>>,
    load: Option<LoadFn<S>>,
    frames: Option<Box<dyn FramePort<S>>>,
    restore_limits: DetReadLimits,
    /// Runs after each step, before the publish, and after a restore (the desync record).
    after_step: Option<AfterStep<S>>,
}

/// Why the host thread calls its [`AfterStep`] hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recorded {
    /// The sim stepped one tick.
    Step,
    /// A restore replaced the sim: the rows from the old timeline are void.
    Restore,
}

/// What the host thread runs after each step, before the publish, and after a restore (the desync
/// record).
type AfterStep<S> = Box<dyn FnMut(&S, Recorded) + Send>;

/// A desync feed for `sim` with its boot row recorded: the host hook and the handle's reader. After a
/// restore to tick `T` the hook clears the ring and records the restored state under `T` (ADR-8621),
/// so no row of the old timeline is left to compare.
fn desync_hook<S: FixedStepSim + SystemFold<N>, const N: usize>(
    sim: &S,
    raw: bool,
) -> (AfterStep<S>, SimHostDesync) {
    let feed = DesyncFeed::<N>::new(S::SYSTEMS, raw);
    feed.record(sim.tick(), sim);
    let reader = feed.reader();
    let hook: AfterStep<S> = Box::new(move |sim: &S, why: Recorded| match why {
        Recorded::Step => feed.record(sim.tick(), sim),
        Recorded::Restore => feed.restart(sim.tick(), sim),
    });
    (hook, reader)
}

fn save_det<S: FixedStepSim + DetState>(sim: &S) -> Result<Vec<u8>, DetStateError> {
    state::snapshot(sim, sim.tick())
}

fn load_det<S: DetState>(bytes: &[u8], limits: DetReadLimits) -> Result<(u64, S), DetStateError> {
    state::restore_with(bytes, limits)
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// Runs a [`FixedStepSim`] on its own thread and publishes its last two frames.
///
/// Dropping the host stops the thread and joins it. [`ThreadedSimHost::into_sim`] stops it and
/// hands the sim back (a hot-reload unload keeps the sim's state this way).
#[derive(Resource)]
pub struct ThreadedSimHost<S: FixedStepSim> {
    shared: Arc<Shared<S::Frame, S::Event>>,
    thread: Mutex<Option<JoinHandle<S>>>,
}

impl<S: FixedStepSim> ThreadedSimHost<S> {
    /// Publish `sim`'s current state as the boot pair and start stepping it on a new thread, on
    /// merged bridge input. Without snapshots ([`ThreadedSimHost::start_with_snapshots`] adds them);
    /// [`HostPacing::Lockstep`] needs [`ThreadedSimHost::start_lockstep`].
    pub fn start(sim: S, settings: SimHostSettings) -> Result<Self, SimHostError> {
        if matches!(settings.pacing, HostPacing::Lockstep { .. }) {
            return Err(SimHostError::InvalidSettings(
                "lockstep pacing needs ThreadedSimHost::start_lockstep".into(),
            ));
        }
        let caps = Caps {
            save: None,
            load: None,
            frames: None,
            restore_limits: settings.restore_limits,
            after_step: None,
        };
        Self::launch(sim, settings, caps, None, |_| None)
    }

    /// [`ThreadedSimHost::start`] for a sim that implements [`SystemFold`]: after every step (before
    /// the frame is published) the host thread records the sim's per-system vector under
    /// [`FixedStepSim::tick`] into a [`DesyncFeed`] (`raw`: also the raw-bits diagnose vector). The
    /// handle carries the reader ([`SimHostHandle::desync`]), so `sim.desync_rows` answers for this
    /// sim. The boot state is recorded too, under its tick.
    pub fn start_with_desync<const N: usize>(
        sim: S,
        settings: SimHostSettings,
        raw: bool,
    ) -> Result<Self, SimHostError>
    where
        S: SystemFold<N>,
    {
        if matches!(settings.pacing, HostPacing::Lockstep { .. }) {
            return Err(SimHostError::InvalidSettings(
                "lockstep pacing needs ThreadedSimHost::start_lockstep".into(),
            ));
        }
        let (hook, reader) = desync_hook::<S, N>(&sim, raw);
        let caps = Caps {
            save: None,
            load: None,
            frames: None,
            restore_limits: settings.restore_limits,
            after_step: Some(hook),
        };
        Self::launch(sim, settings, caps, Some(reader), |_| None)
    }

    /// Validate, publish the boot pair and spawn the thread. `frames` builds the lockstep frame
    /// port from the host's control block (`None`: merged input).
    fn launch(
        mut sim: S,
        settings: SimHostSettings,
        mut caps: Caps<S>,
        desync: Option<SimHostDesync>,
        frames: impl FnOnce(&Arc<Control>) -> Option<Box<dyn FramePort<S>>>,
    ) -> Result<Self, SimHostError> {
        let lockstep = matches!(settings.pacing, HostPacing::Lockstep { .. });
        let (manual, tick_ns) = match settings.pacing {
            HostPacing::Realtime { tick, .. } | HostPacing::Lockstep { tick: Some(tick) } => {
                if tick.is_zero() {
                    return Err(SimHostError::InvalidSettings("the realtime tick is zero".into()));
                }
                (false, u64::try_from(tick.as_nanos()).unwrap_or(u64::MAX))
            }
            HostPacing::Manual | HostPacing::Lockstep { tick: None } => (true, 0),
        };
        if lockstep && settings.frame_queue_cap == 0 {
            return Err(SimHostError::InvalidSettings(
                "the frame queue cap is zero".into(),
            ));
        }
        let mut boot = S::Frame::default();
        sim.write_frame(&mut boot);
        let boot = Arc::new(boot);
        let tick = sim.tick();
        let now = Instant::now();
        let control = Arc::new(Control {
            gate: Mutex::new(Gate {
                stop: false,
                held: settings.start_held,
                ..Gate::default()
            }),
            manual,
            tick_ns,
            lockstep,
            snapshots_enabled: caps.save.is_some() && caps.load.is_some(),
            running: AtomicBool::new(true),
            curr_tick: AtomicU64::new(tick),
            // The start-up probe: an empty set asks only whether the sim draws (ADR-8562).
            det_debug_supported: AtomicBool::new(
                sim.det_debug(DebugWhat::NONE, &mut DebugRecorder::default()),
            ),
            ..Control::default()
        });
        caps.frames = frames(&control);
        let shared = Arc::new(Shared {
            control,
            desync,
            publish: Publish {
                slot: Mutex::new(PublishedFrames {
                    prev: Arc::clone(&boot),
                    curr: boot,
                    prev_tick: tick,
                    curr_tick: tick,
                    prev_at: now,
                    curr_at: now,
                    sequence: 0,
                }),
                published: Condvar::new(),
            },
            bus: Mutex::new(Bus {
                input: BridgeInput::default(),
            }),
            events: Mutex::new(VecDeque::new()),
            event_cap: settings.event_cap,
        });
        let thread_shared = Arc::clone(&shared);
        let run = HostRun {
            pacing: settings.pacing,
            stale_after: settings.client_stale_after,
            fp_check_every: settings.fp_check_every,
        };
        let thread = thread::Builder::new()
            .name(settings.thread_name.clone())
            .spawn(move || host_loop(sim, &thread_shared, run, caps))
            .map_err(|err| SimHostError::Spawn(err.to_string()))?;
        Ok(Self {
            shared,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// A type-erased handle to this host (hold, grant, stamp, counters).
    pub fn handle(&self) -> SimHostHandle {
        SimHostHandle {
            control: Arc::clone(&self.shared.control),
            desync: self.shared.desync.clone(),
        }
    }

    /// The newest published pair. Never blocks on a step: the lock covers a few pointer copies.
    pub fn published(&self) -> PublishedFrames<S::Frame> {
        lock(&self.shared.publish.slot).clone()
    }

    /// Wait until the pair's sequence passes `after`, then return it. Returns an error once the
    /// host has stopped (or the sim panicked) and the sequence can no longer pass `after`.
    pub fn wait_published(&self, after: u64) -> Result<PublishedFrames<S::Frame>, SimHostError> {
        let control = &self.shared.control;
        let mut slot = lock(&self.shared.publish.slot);
        loop {
            if slot.sequence > after {
                return Ok(slot.clone());
            }
            if !control.running.load(Ordering::Acquire) {
                return Err(if control.panicked.load(Ordering::Acquire) {
                    SimHostError::SimPanicked
                } else {
                    SimHostError::Stopped
                });
            }
            slot = self
                .shared
                .publish
                .published
                .wait(slot)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Merge one presentation frame of input into the bus: the axes replace the last ones, the
    /// edges queue in order until the next tick takes them. A held host drops it, and so does a
    /// lockstep host (its [`LockstepFeed`] frames carry the input).
    pub fn push_input(&self, input: &BridgeInput) {
        let control = &self.shared.control;
        if control.lockstep || control.gate().held {
            control
                .edges_dropped
                .fetch_add(input.edges.len() as u64, Ordering::Relaxed);
            return;
        }
        let mut bus = lock(&self.shared.bus);
        bus.input.move_axes = input.move_axes;
        let room = MAX_PENDING_EDGES.saturating_sub(bus.input.edges.len());
        let take = input.edges.len().min(room);
        bus.input.edges.extend(input.edges.iter().take(take).cloned());
        let dropped = input.edges.len() - take;
        drop(bus);
        if dropped > 0 {
            control.edges_dropped.fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }

    /// Move the queued events into `out`, oldest first.
    pub fn take_events(&self, out: &mut Vec<S::Event>) {
        out.extend(lock(&self.shared.events).drain(..));
    }

    /// Stop the thread and join it. Idempotent; [`Drop`] does the same.
    pub fn stop(&self) -> Result<(), SimHostError> {
        self.join().map(drop)
    }

    /// Stop the thread, join it and return the sim (its state as of the last step).
    pub fn into_sim(self) -> Result<S, SimHostError> {
        self.join()?.ok_or(SimHostError::Stopped)
    }

    fn join(&self) -> Result<Option<S>, SimHostError> {
        let control = &self.shared.control;
        control.gate().stop = true;
        control.wake.notify_all();
        let handle = lock(&self.thread).take();
        let Some(handle) = handle else {
            return if control.panicked.load(Ordering::Acquire) {
                Err(SimHostError::SimPanicked)
            } else {
                Ok(None)
            };
        };
        handle.join().map(Some).map_err(|_| SimHostError::SimPanicked)
    }
}

impl<S: FixedStepSim + DetState> ThreadedSimHost<S> {
    /// [`ThreadedSimHost::start`] with snapshots and restore ([`SimHostHandle::request_snapshot`],
    /// [`SimHostHandle::restore`]) through the sim's [`DetState`].
    pub fn start_with_snapshots(sim: S, settings: SimHostSettings) -> Result<Self, SimHostError> {
        if matches!(settings.pacing, HostPacing::Lockstep { .. }) {
            return Err(SimHostError::InvalidSettings(
                "lockstep pacing needs ThreadedSimHost::start_lockstep".into(),
            ));
        }
        let caps = Caps {
            save: Some(save_det::<S>),
            load: Some(load_det::<S>),
            frames: None,
            restore_limits: settings.restore_limits,
            after_step: None,
        };
        Self::launch(sim, settings, caps, None, |_| None)
    }

    /// [`ThreadedSimHost::start_with_snapshots`] that also records the sim's desync rows, as
    /// [`ThreadedSimHost::start_with_desync`] does. A restore to tick `T` clears the ring and records
    /// the restored state under `T`: rows of the old timeline are never compared (ADR-8621).
    pub fn start_with_snapshots_and_desync<const N: usize>(
        sim: S,
        settings: SimHostSettings,
        raw: bool,
    ) -> Result<Self, SimHostError>
    where
        S: SystemFold<N>,
    {
        if matches!(settings.pacing, HostPacing::Lockstep { .. }) {
            return Err(SimHostError::InvalidSettings(
                "lockstep pacing needs ThreadedSimHost::start_lockstep".into(),
            ));
        }
        let (hook, reader) = desync_hook::<S, N>(&sim, raw);
        let caps = Caps {
            save: Some(save_det::<S>),
            load: Some(load_det::<S>),
            frames: None,
            restore_limits: settings.restore_limits,
            after_step: Some(hook),
        };
        Self::launch(sim, settings, caps, Some(reader), |_| None)
    }
}

impl<S: LockstepSim + DetState> ThreadedSimHost<S> {
    /// Start a lockstep host ([`HostPacing::Lockstep`]): it steps only exact per-tick frames pushed
    /// through the returned [`LockstepFeed`], and takes snapshots and restores through the sim's
    /// [`DetState`]. Other pacings are refused (`InvalidSettings`).
    pub fn start_lockstep(
        sim: S,
        settings: SimHostSettings,
    ) -> Result<(Self, LockstepFeed<S::Inputs>), SimHostError> {
        Self::lockstep_inner(sim, settings, None)
    }

    /// [`ThreadedSimHost::start_lockstep`] that also records the sim's desync rows, as
    /// [`ThreadedSimHost::start_with_desync`] does. A restore to tick `T` clears the ring and records
    /// the restored state under `T` (ADR-8621).
    pub fn start_lockstep_with_desync<const N: usize>(
        sim: S,
        settings: SimHostSettings,
        raw: bool,
    ) -> Result<(Self, LockstepFeed<S::Inputs>), SimHostError>
    where
        S: SystemFold<N>,
    {
        let desync = desync_hook::<S, N>(&sim, raw);
        Self::lockstep_inner(sim, settings, Some(desync))
    }

    fn lockstep_inner(
        sim: S,
        settings: SimHostSettings,
        desync: Option<(AfterStep<S>, SimHostDesync)>,
    ) -> Result<(Self, LockstepFeed<S::Inputs>), SimHostError> {
        if !matches!(settings.pacing, HostPacing::Lockstep { .. }) {
            return Err(SimHostError::InvalidSettings(
                "start_lockstep needs HostPacing::Lockstep".into(),
            ));
        }
        let applied = sim.tick();
        let cap = settings.frame_queue_cap;
        let (after_step, reader) = desync.unzip();
        let caps = Caps {
            save: Some(save_det::<S>),
            load: Some(load_det::<S>),
            frames: None,
            restore_limits: settings.restore_limits,
            after_step,
        };
        let mut made = None;
        let host = Self::launch(sim, settings, caps, reader, |control| {
            let queue = Arc::new(FrameQueue {
                slots: Mutex::new(FrameSlots {
                    applied,
                    frames: BTreeMap::new(),
                }),
                control: Arc::clone(control),
                cap,
            });
            made = Some(Arc::clone(&queue));
            Some(Box::new(Port::<S>(queue)) as Box<dyn FramePort<S>>)
        })?;
        let queue = made.ok_or(SimHostError::Stopped)?;
        Ok((host, LockstepFeed { queue }))
    }
}

impl<S: FixedStepSim> Drop for ThreadedSimHost<S> {
    fn drop(&mut self) {
        // A panicked sim has already been reported through the status; nothing to add here.
        let _ = self.join();
    }
}

impl<S: FixedStepSim> ExternalSim for ThreadedSimHost<S>
where
    S::Frame: SimSnapshot + Clone,
{
    type Snapshot = S::Frame;
    type Event = S::Event;

    fn submit_input(&mut self, input: &BridgeInput) {
        self.push_input(input);
    }

    /// Copies the newest frame (`curr`). The bridge blends through
    /// [`read_pair`](ExternalSim::read_pair) instead; game code can read
    /// [`ThreadedSimHost::published`].
    fn read_snapshot(&mut self, _frame_time: Duration, out: &mut Self::Snapshot) -> SnapshotTick {
        let pair = self.published();
        out.clone_from(&pair.curr);
        SnapshotTick(pair.curr_tick)
    }

    /// The published pair, from one publish (ADR-8456). A realtime host returns the weight
    /// [`PublishedFrames::alpha`] gives now, `delay` behind the newest publish. A manual host has
    /// no clock to blend on (its publish times are when grants ran), and a still pair has nothing
    /// to blend: both return a weight of 1 and leave `prev` unfilled.
    fn read_pair(
        &mut self,
        _frame_time: Duration,
        delay: Duration,
        prev: &mut Self::Snapshot,
        curr: &mut Self::Snapshot,
    ) -> Option<SnapshotPair> {
        let pair = self.published();
        curr.clone_from(&pair.curr);
        let alpha = if self.shared.control.manual || pair.is_still() {
            1.0
        } else {
            pair.alpha(Instant::now(), delay)
        };
        if alpha < crate::sim_blend::BLEND_CURR_AT {
            prev.clone_from(&pair.prev);
        }
        Some(SnapshotPair {
            prev_tick: SnapshotTick(pair.prev_tick),
            curr_tick: SnapshotTick(pair.curr_tick),
            alpha,
        })
    }

    fn drain_events(&mut self, out: &mut Vec<Self::Event>) {
        self.take_events(out);
    }
}

// ---------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------

/// What the realtime clock does on one wake ([`plan_wake`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakePlan {
    /// Sleep this long before stepping (`None`: step now).
    pub sleep: Option<Duration>,
    /// The deadline after this wake's decision (before the step adds a tick).
    pub next: Instant,
    /// Consecutive late ticks after this wake.
    pub behind: u32,
    /// Whole ticks of debt dropped on this wake (0 when the clock kept up or caught up).
    pub dropped: u64,
}

/// The realtime clock's decision on one wake: on time, sleep until `next`; late with catch-up
/// left, step at once; late past `catch_up` ticks in a row, drop the debt and restart the clock
/// at `now`. Pure, so the policy is testable with synthetic instants.
pub fn plan_wake(now: Instant, next: Instant, behind: u32, catch_up: u32, tick: Duration) -> WakePlan {
    if let Some(wait) = next.checked_duration_since(now).filter(|wait| !wait.is_zero()) {
        return WakePlan {
            sleep: Some(wait),
            next,
            behind: 0,
            dropped: 0,
        };
    }
    if behind < catch_up {
        return WakePlan {
            sleep: None,
            next,
            behind: behind + 1,
            dropped: 0,
        };
    }
    let late = now.saturating_duration_since(next);
    let dropped = if tick.is_zero() {
        0
    } else {
        u64::try_from(late.as_nanos() / tick.as_nanos()).unwrap_or(u64::MAX)
    };
    WakePlan {
        sleep: None,
        next: now,
        behind: 0,
        dropped,
    }
}

/// Whether a presentation stamp `beat_ms` (0: none yet) is fresh at `now_ms`.
pub fn client_is_fresh(beat_ms: u64, now_ms: u64, stale_after: Duration) -> bool {
    let limit = u64::try_from(stale_after.as_millis()).unwrap_or(u64::MAX);
    beat_ms != 0 && now_ms.saturating_sub(beat_ms) <= limit
}

/// Clears `running` and wakes every waiter when the thread ends, including by a panic.
struct RunningGuard<'a, F, E> {
    shared: &'a Shared<F, E>,
}

impl<F, E> Drop for RunningGuard<'_, F, E> {
    fn drop(&mut self) {
        let control = &self.shared.control;
        if thread::panicking() {
            control.panicked.store(true, Ordering::Release);
        }
        // Take the slot lock so a reader between its check and its wait cannot miss the wake.
        let slot = lock(&self.shared.publish.slot);
        control.running.store(false, Ordering::Release);
        drop(slot);
        // Commands nobody will run: dropping their reply senders answers `Stopped`.
        let orphaned = std::mem::take(&mut control.gate().commands);
        drop(orphaned);
        self.shared.publish.published.notify_all();
        control.wake.notify_all();
        control.bump();
    }
}

/// What the gate allows on one wake.
enum Wake {
    Stop,
    Step,
    /// A snapshot or restore is queued: run it before deciding again.
    Command,
    /// Step, having taken one manual grant (handed back if a hold lands before the step).
    Granted,
    /// Held before a manual grant was taken: go round and wait out the hold.
    Held,
}

/// How the thread runs (copied out of the settings).
#[derive(Debug, Clone, Copy)]
struct HostRun {
    pacing: HostPacing,
    stale_after: Option<Duration>,
    fp_check_every: u32,
}

/// The thread-start and periodic floating-point environment guard.
struct FpGuard<'a> {
    control: &'a Control,
    every: u32,
    since: u32,
}

impl<'a> FpGuard<'a> {
    /// Put the thread's environment back to the IEEE default before anything else runs on it.
    fn start(control: &'a Control, every: u32) -> Self {
        let report = fpu::ensure_default_fp_env();
        control.fp_supported.store(report.supported, Ordering::Release);
        control.fp_reset.store(!report.was_default(), Ordering::Release);
        control.fp_at_start.store(report.raw_before, Ordering::Release);
        if !report.was_default() {
            tracing::warn!(
                raw = report.raw_before,
                "sim host thread started with a non-default floating-point environment; reset it"
            );
        }
        control.bump();
        Self {
            control,
            every,
            since: 0,
        }
    }

    /// Every `every` steps: check the environment again and put it back if foreign code changed it.
    fn before_step(&mut self) {
        if self.every == 0 {
            return;
        }
        self.since += 1;
        if self.since < self.every {
            return;
        }
        self.since = 0;
        let report = fpu::ensure_default_fp_env();
        self.control.fp_checks.fetch_add(1, Ordering::AcqRel);
        if !report.was_default() {
            self.control.fp_changed.fetch_add(1, Ordering::AcqRel);
            tracing::warn!(
                raw = report.raw_before,
                "the sim host thread's floating-point environment changed; reset it"
            );
        }
    }
}

/// A snapshot asked for a tick the sim has not reached yet.
struct PendingSnapshot {
    at: u64,
    reply: mpsc::Sender<Result<HostSnapshot, SimHostError>>,
}

/// The thread's scratch: a recycled frame buffer, the event buffer and pending snapshots.
struct StepState<F, E> {
    spare: Option<F>,
    events: Vec<E>,
    pending: Vec<PendingSnapshot>,
}

fn host_loop<S: FixedStepSim>(
    mut sim: S,
    shared: &Shared<S::Frame, S::Event>,
    run: HostRun,
    mut caps: Caps<S>,
) -> S {
    let _running = RunningGuard { shared };
    let control = &shared.control;
    // First thing on the thread: the sim must never step under an inherited FTZ or rounding mode.
    let mut fp = FpGuard::start(control, run.fp_check_every);
    let (tick, catch_up, by_grant) = match run.pacing {
        HostPacing::Realtime { tick, catch_up } => (tick, catch_up, false),
        HostPacing::Manual => (Duration::ZERO, 0, true),
        HostPacing::Lockstep { tick: Some(tick) } => (tick, 0, false),
        HostPacing::Lockstep { tick: None } => (Duration::ZERO, 0, true),
    };
    let mut next = Instant::now() + tick;
    let mut behind = 0u32;
    let mut st = StepState {
        spare: None,
        events: Vec::new(),
        pending: Vec::new(),
    };
    let mut waiting_client = false;
    loop {
        if run_commands(&mut sim, shared, &mut caps, &mut st) {
            // A restore restarts the clock: the time it took is not debt.
            next = Instant::now() + tick;
            behind = 0;
        }
        // Held: publish a still pair once, then sleep until released, stopped or sent a command.
        match wait_while_held(shared) {
            HoldEnd::Stop => break,
            HoldEnd::Command => continue,
            // A release restarts the clock: the held time is not debt.
            HoldEnd::Released => {
                next = Instant::now() + tick;
                behind = 0;
            }
            HoldEnd::NotHeld => {}
        }
        let wake = if by_grant {
            wait_for_grant(control)
        } else {
            let plan = plan_wake(Instant::now(), next, behind, catch_up, tick);
            if plan.dropped > 0 {
                control.debt_drops.fetch_add(1, Ordering::Relaxed);
                control.ticks_dropped.fetch_add(plan.dropped, Ordering::Relaxed);
            }
            next = plan.next;
            behind = plan.behind;
            match plan.sleep {
                Some(wait) => match sleep_until(control, Instant::now() + wait) {
                    // Woken early (held, released, a command or a stop): decide again.
                    SleepEnd::Interrupted => continue,
                    SleepEnd::Stop => Wake::Stop,
                    SleepEnd::Deadline => Wake::Step,
                },
                None => {
                    if control.gate().stop {
                        Wake::Stop
                    } else {
                        Wake::Step
                    }
                }
            }
        };
        match wake {
            Wake::Stop => break,
            Wake::Command | Wake::Held => continue,
            Wake::Step | Wake::Granted => {}
        }
        #[cfg(test)]
        if matches!(wake, Wake::Granted) && control.hold_after_grant.swap(false, Ordering::AcqRel) {
            control.gate().held = true;
        }
        {
            let mut gate = control.gate();
            if gate.held {
                // A hold that landed after this wake took a grant keeps the grant for the release
                // (it was dropped before ADR-8456's and ADR-8547's fix, and a granted run never
                // finished): a hold never eats a granted tick.
                if matches!(wake, Wake::Granted) {
                    gate.granted = gate.granted.saturating_add(1);
                }
                continue;
            }
        }
        if let (Some(stale), false) = (run.stale_after, by_grant) {
            let beat = control.client_beat_ms.load(Ordering::Acquire);
            if !client_is_fresh(beat, control.now_ms(), stale) {
                if !waiting_client {
                    waiting_client = true;
                    tracing::info!("sim host paused: no fresh presentation frame");
                }
                control.client_waits.fetch_add(1, Ordering::Relaxed);
                next = Instant::now() + tick;
                continue;
            }
            if waiting_client {
                waiting_client = false;
                tracing::info!("sim host resumed: the presentation frame is fresh");
            }
        }
        match &caps.frames {
            None => {
                let input = {
                    let mut bus = lock(&shared.bus);
                    let edges = std::mem::take(&mut bus.input.edges);
                    BridgeInput {
                        move_axes: bus.input.move_axes,
                        edges,
                    }
                };
                step_and_publish(
                    &mut sim,
                    shared,
                    &mut st,
                    &mut fp,
                    caps.save,
                    caps.after_step.as_mut(),
                    |sim| {
                        sim.step(&input);
                        true
                    },
                );
                next += tick;
            }
            Some(port) => match step_frames(
                &mut sim,
                shared,
                port.as_ref(),
                &mut st,
                &mut fp,
                caps.save,
                caps.after_step.as_mut(),
            ) {
                FrameWake::Stepped => next += tick,
                // The wait for a missing frame is not clock debt.
                FrameWake::SteppedAfterStall => {
                    next = Instant::now() + tick;
                    behind = 0;
                }
                FrameWake::Stop => break,
                FrameWake::Interrupted => {
                    // The wake stepped nothing: a grant goes back, and the stall is not debt.
                    if by_grant {
                        let mut gate = control.gate();
                        gate.granted = gate.granted.saturating_add(1);
                    }
                    next = Instant::now() + tick;
                    behind = 0;
                }
            },
        }
    }
    sim
}

/// Record the sim's det debug frame while the switch is on; drop it once when it goes off
/// (ADR-8562). The sim draws under the frame lock into the stored recorder, so a step allocates
/// only while its frame grows.
fn det_debug_after_step<S: FixedStepSim>(sim: &mut S, control: &Control, tick: u64) {
    if !control.det_debug_supported.load(Ordering::Relaxed) {
        return;
    }
    let what = DebugWhat::from_bits(control.det_debug_what.load(Ordering::Relaxed));
    let mut slot = lock(&control.det_debug_frame);
    if what.is_empty() {
        if slot.is_some() {
            // Let the sim turn capture off, then forget the frame.
            let mut scratch = DebugRecorder::default();
            sim.det_debug(what, &mut scratch);
            *slot = None;
        }
        return;
    }
    let _span = tracing::trace_span!("sim_host.det_debug").entered();
    let (at, frame) = slot.get_or_insert_with(|| (tick, DebugRecorder::default()));
    *at = tick;
    frame.clear();
    sim.det_debug(what, frame);
}

enum FrameWake {
    Stepped,
    SteppedAfterStall,
    Stop,
    Interrupted,
}

/// One lockstep wake: wait for the next tick's frame if it is missing (a stall), then step one
/// frame, or up to the catch-up budget of queued frames back to back.
fn step_frames<S: FixedStepSim>(
    sim: &mut S,
    shared: &Shared<S::Frame, S::Event>,
    port: &dyn FramePort<S>,
    st: &mut StepState<S::Frame, S::Event>,
    fp: &mut FpGuard<'_>,
    save: Option<SaveFn<S>>,
    mut after_step: Option<&mut AfterStep<S>>,
) -> FrameWake {
    let control = &shared.control;
    let first = sim.tick().wrapping_add(1);
    let stalled = !port.has(first);
    if stalled {
        control.frame_stalls.fetch_add(1, Ordering::AcqRel);
        control.bump();
        match wait_for_frame(control, port, first) {
            FrameWait::Ready => {}
            FrameWait::Stop => return FrameWake::Stop,
            FrameWait::Interrupted => return FrameWake::Interrupted,
        }
    }
    let budget = control.gate().catch_up.max(1);
    let mut stepped = 0u32;
    while stepped < budget {
        if !port.has(sim.tick().wrapping_add(1)) {
            break;
        }
        if stepped > 0 {
            // Counted before the tick is published, so a status that shows the tick shows this.
            control.catch_up_ticks.fetch_add(1, Ordering::AcqRel);
        }
        if !step_and_publish(sim, shared, st, fp, save, after_step.as_deref_mut(), |sim| {
            port.step_next(sim)
        }) {
            break;
        }
        stepped += 1;
        if stepped < budget {
            // A stop, a hold or a command ends a catch-up burst early; the rest waits a wake.
            let gate = control.gate();
            if gate.stop || gate.held || !gate.commands.is_empty() {
                break;
            }
        }
    }
    if budget > 1 && !port.has(sim.tick().wrapping_add(1)) {
        // The next tick's frame has not arrived: the host is at the live edge, catch-up is done.
        let mut gate = control.gate();
        gate.catch_up = 0;
        control.catch_up_budget.store(0, Ordering::Release);
        drop(gate);
        control.bump();
    }
    if stepped == 0 {
        FrameWake::Interrupted
    } else if stalled {
        FrameWake::SteppedAfterStall
    } else {
        FrameWake::Stepped
    }
}

/// One tick: run `step` (it returns `false` when it had nothing to step), write the frame, queue
/// events, publish the pair, then answer snapshots asked for this tick.
fn step_and_publish<S: FixedStepSim>(
    sim: &mut S,
    shared: &Shared<S::Frame, S::Event>,
    st: &mut StepState<S::Frame, S::Event>,
    fp: &mut FpGuard<'_>,
    save: Option<SaveFn<S>>,
    after_step: Option<&mut AfterStep<S>>,
    step: impl FnOnce(&mut S) -> bool,
) -> bool {
    let _span = tracing::info_span!("sim_host.tick").entered();
    let control = &shared.control;
    fp.before_step();
    let started = Instant::now();
    if !step(sim) {
        return false;
    }
    let step_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    control.last_step_ns.store(step_ns, Ordering::Relaxed);
    control.max_step_ns.fetch_max(step_ns, Ordering::Relaxed);
    // The desync record runs before the publish, so a reader that sees the tick sees its row.
    if let Some(after_step) = after_step {
        after_step(sim, Recorded::Step);
    }
    // Stored before the counters move, so a reader that sees the new tick sees this step's report.
    *lock(&control.tick_profile) = sim.tick_profile();
    let tick = sim.tick();
    det_debug_after_step(sim, control, tick);
    let mut frame = st.spare.take().unwrap_or_default();
    sim.write_frame(&mut frame);
    let frame = Arc::new(frame);

    sim.drain_events(&mut st.events);
    if !st.events.is_empty() {
        let mut queue = lock(&shared.events);
        queue.extend(st.events.drain(..));
        let over = queue.len().saturating_sub(shared.event_cap);
        if over > 0 {
            queue.drain(..over);
            control.events_dropped.fetch_add(over as u64, Ordering::Relaxed);
        }
    }

    let now = Instant::now();
    let mut slot = lock(&shared.publish.slot);
    let newest = Arc::clone(&slot.curr);
    let old_prev = std::mem::replace(&mut slot.prev, newest);
    slot.prev_tick = slot.curr_tick;
    slot.prev_at = slot.curr_at;
    slot.curr = frame;
    slot.curr_tick = tick;
    slot.curr_at = now;
    slot.sequence += 1;
    // The counters move with the pair, before any waiter wakes: a reader that saw this publish
    // never reads an older status.
    control.ticks.fetch_add(1, Ordering::AcqRel);
    control.curr_tick.store(tick, Ordering::Release);
    control.publishes.store(slot.sequence, Ordering::Release);
    drop(slot);
    shared.publish.published.notify_all();
    // Reuse the dropped frame's allocation when no reader holds it any more.
    st.spare = Arc::try_unwrap(old_prev).ok();
    serve_pending(sim, control, save, &mut st.pending);
    control.bump();
    true
}

/// Snapshot the sim now, on the host thread.
fn take_snapshot<S: FixedStepSim>(
    sim: &S,
    control: &Control,
    save: Option<SaveFn<S>>,
) -> Result<HostSnapshot, SimHostError> {
    let save = save.ok_or(SimHostError::NoSnapshots)?;
    let _span = tracing::info_span!("sim_host.snapshot", tick = sim.tick()).entered();
    let bytes = save(sim).map_err(SimHostError::SnapshotFailed)?;
    let limits = DetReadLimits {
        max_bytes: usize::MAX,
        max_len: u32::MAX,
    };
    let header = state::read_header(&bytes, limits).map_err(SimHostError::SnapshotFailed)?;
    control.snapshots.fetch_add(1, Ordering::AcqRel);
    control.bump();
    Ok(HostSnapshot {
        tick: header.tick,
        header,
        bytes,
    })
}

/// Answer the pending snapshots the sim has reached (or passed, after a restore).
fn serve_pending<S: FixedStepSim>(
    sim: &S,
    control: &Control,
    save: Option<SaveFn<S>>,
    pending: &mut Vec<PendingSnapshot>,
) {
    if pending.is_empty() {
        return;
    }
    let now = sim.tick();
    pending.retain(|request| {
        if request.at > now {
            return true;
        }
        let answer = if request.at == now {
            take_snapshot(sim, control, save)
        } else {
            Err(SimHostError::SnapshotTickPassed {
                tick: request.at,
                curr: now,
            })
        };
        // A requester that dropped its reply no longer wants the answer.
        let _ = request.reply.send(answer);
        false
    });
}

/// Run every queued command; `true` when a restore replaced the sim.
fn run_commands<S: FixedStepSim>(
    sim: &mut S,
    shared: &Shared<S::Frame, S::Event>,
    caps: &mut Caps<S>,
    st: &mut StepState<S::Frame, S::Event>,
) -> bool {
    let control = &shared.control;
    let mut restored = false;
    loop {
        let command = control.gate().commands.pop_front();
        let Some(command) = command else {
            break;
        };
        match command {
            Command::Snapshot { at, reply } => {
                let now = sim.tick();
                match at {
                    Some(at) if at > now => st.pending.push(PendingSnapshot { at, reply }),
                    Some(at) if at < now => {
                        let _ = reply.send(Err(SimHostError::SnapshotTickPassed { tick: at, curr: now }));
                    }
                    _ => {
                        let _ = reply.send(take_snapshot(sim, control, caps.save));
                    }
                }
            }
            Command::Restore { bytes, reply } => {
                let answer = restore_sim(sim, shared, caps, st, &bytes);
                restored |= answer.is_ok();
                let _ = reply.send(answer);
            }
        }
    }
    restored
}

/// Load `bytes` through the sim's `DetState`, replace the sim, drop stale input and frames, restart
/// the desync ring at the restored tick, and publish a still boot pair at that tick. Refused bytes
/// leave everything unchanged.
fn restore_sim<S: FixedStepSim>(
    sim: &mut S,
    shared: &Shared<S::Frame, S::Event>,
    caps: &mut Caps<S>,
    st: &mut StepState<S::Frame, S::Event>,
    bytes: &[u8],
) -> Result<u64, SimHostError> {
    let control = &shared.control;
    let load = caps.load.ok_or(SimHostError::NoSnapshots)?;
    let _span = tracing::info_span!("sim_host.restore", len = bytes.len()).entered();
    let (tick, loaded) = load(bytes, caps.restore_limits).map_err(SimHostError::RestoreFailed)?;
    if loaded.tick() != tick {
        return Err(SimHostError::RestoreFailed(DetStateError::bad_value(
            format_args!(
                "the snapshot is framed at tick {tick} but the loaded sim reports tick {}",
                loaded.tick()
            ),
        )));
    }
    *sim = loaded;
    // Merged input offered before the restore belongs to the old timeline.
    lock(&shared.bus).input = BridgeInput::default();
    if let Some(port) = &caps.frames {
        port.rebase(tick);
    }
    // The ring's rows belong to the old timeline: clear it and record the restored state (before
    // the publish, so a reader that sees the restored tick sees its row).
    if let Some(after_step) = caps.after_step.as_mut() {
        after_step(sim, Recorded::Restore);
    }
    let mut frame = st.spare.take().unwrap_or_default();
    sim.write_frame(&mut frame);
    let frame = Arc::new(frame);
    let now = Instant::now();
    let mut slot = lock(&shared.publish.slot);
    slot.prev = Arc::clone(&frame);
    slot.curr = frame;
    slot.prev_tick = tick;
    slot.curr_tick = tick;
    slot.prev_at = now;
    slot.curr_at = now;
    slot.sequence += 1;
    control.curr_tick.store(tick, Ordering::Release);
    control.publishes.store(slot.sequence, Ordering::Release);
    drop(slot);
    shared.publish.published.notify_all();
    control.restores.fetch_add(1, Ordering::AcqRel);
    serve_pending(sim, control, caps.save, &mut st.pending);
    control.bump();
    Ok(tick)
}

enum HoldEnd {
    Stop,
    Released,
    NotHeld,
    /// A command arrived while held: run it, then hold again.
    Command,
}

/// While held, make the published pair still (once) and wait for a release, a stop or a command.
fn wait_while_held<F, E>(shared: &Shared<F, E>) -> HoldEnd {
    let control = &shared.control;
    let mut gate = control.gate();
    if gate.stop {
        return HoldEnd::Stop;
    }
    if !gate.held {
        return HoldEnd::NotHeld;
    }
    drop(gate);
    {
        let mut slot = lock(&shared.publish.slot);
        if !Arc::ptr_eq(&slot.prev, &slot.curr) {
            let now = Instant::now();
            slot.prev = Arc::clone(&slot.curr);
            slot.prev_tick = slot.curr_tick;
            slot.prev_at = now;
            slot.curr_at = now;
            slot.sequence += 1;
            control.publishes.store(slot.sequence, Ordering::Release);
            drop(slot);
            shared.publish.published.notify_all();
            control.bump();
        }
    }
    // Input offered before the hold took effect is dropped with it.
    let dropped = {
        let mut bus = lock(&shared.bus);
        let edges = std::mem::take(&mut bus.input.edges);
        bus.input.move_axes = Default::default();
        edges.len()
    };
    if dropped > 0 {
        control.edges_dropped.fetch_add(dropped as u64, Ordering::Relaxed);
    }
    gate = control.gate();
    while gate.held && !gate.stop && gate.commands.is_empty() {
        gate = control.wake.wait(gate).unwrap_or_else(PoisonError::into_inner);
    }
    if gate.stop {
        HoldEnd::Stop
    } else if gate.held {
        HoldEnd::Command
    } else {
        HoldEnd::Released
    }
}

fn wait_for_grant(control: &Control) -> Wake {
    let mut gate = control.gate();
    loop {
        if gate.stop {
            return Wake::Stop;
        }
        if !gate.commands.is_empty() {
            return Wake::Command;
        }
        if gate.held {
            // Let the loop publish the still pair and wait for the release (no grant taken).
            return Wake::Held;
        }
        if gate.granted > 0 {
            gate.granted -= 1;
            return Wake::Granted;
        }
        gate = control.wake.wait(gate).unwrap_or_else(PoisonError::into_inner);
    }
}

enum FrameWait {
    Ready,
    Stop,
    Interrupted,
}

/// A lockstep stall: wait until the frame for `tick` is queued, or a stop, hold or command.
fn wait_for_frame<S>(control: &Control, port: &dyn FramePort<S>, tick: u64) -> FrameWait {
    let mut gate = control.gate();
    loop {
        if gate.stop {
            return FrameWait::Stop;
        }
        if gate.held || !gate.commands.is_empty() {
            return FrameWait::Interrupted;
        }
        // Lock order: gate, then the frame queue (a push releases the queue before the gate).
        if port.has(tick) {
            return FrameWait::Ready;
        }
        gate = control.wake.wait(gate).unwrap_or_else(PoisonError::into_inner);
    }
}

enum SleepEnd {
    Deadline,
    Interrupted,
    Stop,
}

/// Sleep until `deadline`; a stop, a hold or a command wakes it early.
fn sleep_until(control: &Control, deadline: Instant) -> SleepEnd {
    let mut gate = control.gate();
    loop {
        if gate.stop {
            return SleepEnd::Stop;
        }
        if gate.held || !gate.commands.is_empty() {
            return SleepEnd::Interrupted;
        }
        let Some(left) = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            return SleepEnd::Deadline;
        };
        gate = control
            .wake
            .wait_timeout(gate, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

#[cfg(test)]
mod tests;
