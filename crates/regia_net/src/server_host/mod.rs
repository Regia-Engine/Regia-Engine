// SPDX-License-Identifier: MPL-2.0
//! The dedicated-server runtime host (spec 76 § 4-§ 9): configuration, tick pacing, startup,
//! graceful shutdown and reload, the operator console, remote admin and the health endpoint.
//!
//! | State | What happens | Leaves to |
//! |---|---|---|
//! | `Starting` | config resolved; log dir; `content/net-manifest.ron`; `bans.ron`; game port bound | `Loading` (or exit 2/3/4) |
//! | `Loading` | the rotation's current level loads | `Running` (or exit 4) |
//! | `Running` | sessions; one fixed step per tick; level rotation at match end | `Draining` |
//! | `Draining` | new joins refused; every client told `server_shutting_down`; up to the grace to flush | `Stopped` |
//! | `Stopped` | exit code 0 | — |
//!
//! The host reads time through [`HostClock`], so pacing is tested with a [`TestClock`] and no wall
//! clock. The game plugs in through `HostGame`: `SessionOnlyGame` (a session and nothing else)
//! or `AppHostGame` (a `regia_app::App` with `NetSessionPlugin`).
//!
//! A replication game (a `NetLink` app, the `net-game` template) adds a
//! `HostedLink` resource: the link then binds the game port and the session runs local
//! (`link_game`, ADR-2440). `--relay` mints a join code for the game port ([`join`]).
//!
//! [`join`] and [`relay_path`] (a join ticket, the relay registrar, `relay_split`, `JoinPath` and
//! `JoinSocket`) and [`TestClock`] compile with `default-features = false` and no `ecs`, so a game
//! that runs its own simulation can register with the relay and drive a member's
//! [`crate::lobby::hub::JoinPlanDriver`] without linking `regia_ecs` (ADR-10376). `HostGame` and
//! the `ServerHost` runtime stay behind `ecs`.

#[cfg(feature = "ecs")]
pub mod config;
#[cfg(feature = "ecs")]
pub mod crash;
#[cfg(feature = "ecs")]
mod ecs_host;
#[cfg(feature = "ecs")]
pub mod health;
pub mod join;
#[cfg(feature = "ecs")]
pub mod link_admin;
#[cfg(feature = "ecs")]
pub mod link_game;
#[cfg(feature = "ecs")]
pub mod lockstep_game;
/// The per-OS code (docs/04-conventions.md § 8, rc1-rev-net-k 12).
#[cfg(feature = "ecs")]
mod platform;
pub mod relay_path;
#[cfg(feature = "ecs")]
pub mod shared_port;
#[cfg(feature = "ecs")]
pub mod signals;

#[cfg(feature = "ecs")]
pub use ecs_host::*;

use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(feature = "ecs")]
pub use config::{
    CliOverrides, FieldError, PersistenceSettings, PlayerIdentityMode, ResolvedConfig, ServerBotSettings,
    ServerSettings, resolve_config,
};
#[cfg(feature = "ecs")]
pub use crash::{CrashCause, crash_exit_code, install_native_crash_exit};
#[cfg(feature = "ecs")]
pub use health::{HealthServer, HealthSnapshot};
pub use join::{
    JoinError, JoinResolver, JoinTicket, LOCAL_RELAY_ALLOCATION_PORTS, LocalRelay, MappedCandidates,
    RelayRegistrar, RelaySpec, resolve_join_code,
};
#[cfg(feature = "ecs")]
pub use link_admin::{
    LinkBanMirror, PlayerLayer, ban_link_peer, enforce_link_bans, kick_link_peer, link_hosts_game_port,
    player_layer, player_layer_in_world, unban_link_entry,
};
#[cfg(feature = "ecs")]
pub use link_game::{GamePort, HostedLink};
#[cfg(feature = "ecs")]
pub use lockstep_game::{LockstepHostGame, LockstepSeatLink};
pub use relay_path::{
    JoinPath, JoinRoute, JoinSocket, RELAY_FALLBACK_MS, RELAY_RACE_START_MS, RelayControlPort, RelayGamePort,
    RelaySplitStats, fresh_allocation_key, relay_split,
};
#[cfg(feature = "ecs")]
pub use shared_port::{PortHalf, PortLayer, SharedPort, SharedPortStats};
#[cfg(feature = "ecs")]
pub use signals::HostSignals;

/// Process exit codes (spec 76 § 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// Normal shutdown.
    Ok = 0,
    /// Configuration or command-line error.
    Config = 2,
    /// A port could not be bound.
    Bind = 3,
    /// Content error (pak, manifest or level).
    Content = 4,
    /// Crash (written by the crash handler).
    Crash = 70,
}

impl ExitCode {
    /// The numeric code.
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// Why the host stopped before running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    /// The exit code.
    pub exit: ExitCode,
    /// One line per problem.
    pub lines: Vec<String>,
}

impl HostError {
    // Only the ECS host builds a `HostError`. Without `ecs` the constructor is unused, and
    // `dead_code` is denied (ADR-10376).
    #[cfg(feature = "ecs")]
    fn new(exit: ExitCode, line: impl Into<String>) -> Self {
        Self {
            exit,
            lines: vec![line.into()],
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit {}: {}", self.exit.code(), self.lines.join("; "))
    }
}

/// The host's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// Reading config, content, bans; binding.
    Starting,
    /// Loading the first level.
    Loading,
    /// Serving.
    Running,
    /// Shutting down gracefully.
    Draining,
    /// Done.
    Stopped,
}

impl HostState {
    /// The lowercase name (log phase markers, `/health`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Loading => "loading",
            Self::Running => "running",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
        }
    }
}

/// Where the host reads time.
pub trait HostClock: Send {
    /// Time since the clock's start.
    fn now(&self) -> Duration;
    /// Waits until `at` (no busy-waiting).
    fn sleep_until(&mut self, at: Duration);
}

/// The monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicClock {
    start: std::time::Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl HostClock for MonotonicClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep_until(&mut self, at: Duration) {
        let now = self.now();
        if at > now {
            std::thread::sleep(at - now);
        }
    }
}

/// A clock tests drive: `sleep_until` jumps to the deadline; [`TestClock::advance`] moves it (from
/// inside a game step, to simulate a slow tick). Clones share the time.
#[derive(Debug, Clone, Default)]
pub struct TestClock {
    now: Arc<Mutex<Duration>>,
}

impl TestClock {
    /// A clock at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the clock forward.
    pub fn advance(&self, d: Duration) {
        if let Ok(mut n) = self.now.lock() {
            *n += d;
        }
    }
}

impl HostClock for TestClock {
    fn now(&self) -> Duration {
        self.now.lock().map(|n| *n).unwrap_or_default()
    }

    fn sleep_until(&mut self, at: Duration) {
        if let Ok(mut n) = self.now.lock()
            && at > *n
        {
            *n = at;
        }
    }
}
