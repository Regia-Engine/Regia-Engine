// SPDX-License-Identifier: MPL-2.0
//! The ECS half of the dedicated-server host (spec 76, feature `ecs`, ADR-10376).
//!
//! `ServerHost`, `HostGame` and the operator console live here. The relay helpers and `TestClock`
//! stay on the parent module so a build without `ecs` can still use them. This file is a child of
//! `server_host`; the parent re-exports its public items.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::App;
use regia_ecs::prelude::{Resource, World, bevy_ecs};

use super::{
    CliOverrides, ExitCode, GamePort, HealthServer, HealthSnapshot, HostClock, HostError, HostSignals,
    HostState, HostedLink, JoinTicket, LocalRelay, PlayerLayer, RelayControlPort, RelayRegistrar, RelaySpec,
    ServerSettings, SharedPort, TestClock, ban_link_peer, enforce_link_bans, link_hosts_game_port,
    player_layer, relay_split, resolve_config, unban_link_entry,
};
use crate::crypto::{ServerIdentity, ServerKey};
use crate::model::{NetPlayerId, PeerId};
use crate::session::bans::{BanId, BanKind, BanStore};
use crate::session::lifecycle::{HostOptions, SessionSettings};
use crate::session::manifest::ContentManifest;
use crate::session::net_session::SessionError;
use crate::session::{NetSession, SessionRules};

/// A lifecycle point of the host that a game layer acts on: the world persistence of
/// `regia_framework::server_persistence` (ADR-3354). Delivered by [`HostGame::host_event`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum HostEvent<'a> {
    /// The first level loaded and no session update has run yet, so no client is in: the place to
    /// restore a saved world. An `Err` stops the start with exit code 4.
    Started {
        /// The level.
        level: &'a str,
        /// The server directory (relative paths of the settings resolve against it).
        base_dir: &'a Path,
        /// The resolved settings.
        settings: &'a ServerSettings,
    },
    /// One host tick ran (`tick` counts from 1; idle-slowdown ticks count too).
    Ticked {
        /// The host tick just finished.
        tick: u64,
    },
    /// The host leaves `from` for `to` (console `level`/`next`, a match end, an in-game travel),
    /// before `to` loads.
    LevelLeaving {
        /// The level being left.
        from: &'a str,
        /// The level about to load.
        to: &'a str,
    },
    /// `level` loaded after a level change.
    LevelLoaded {
        /// The level.
        level: &'a str,
    },
    /// A graceful stop: the drain is over (players are gone); the last point to save.
    Stopping {
        /// The last host tick.
        tick: u64,
    },
    /// The playlist row about to load. A game layer copies `mode` and `options` before the level
    /// loads. An empty `mode` leaves the level's own rules.
    RotationRow {
        /// The level path.
        map_id: &'a str,
        /// Mode id, or empty for the level's own rules.
        mode: &'a str,
        /// Row options as borrowed pairs (key, value).
        options: &'a [(&'a str, &'a str)],
    },
    /// `next_level` could not load this row and will try the following one.
    NextLevelFailed {
        /// The level path that failed.
        map_id: &'a str,
        /// The row's mode id.
        mode: &'a str,
        /// Why the row was skipped.
        error: &'a str,
    },
}

/// A host-event hook of an [`AppHostGame`]: the world and the event. An `Err` on
/// [`HostEvent::Started`] stops the start; on the other events it is logged.
pub type HostEventHook = fn(&mut World, &HostEvent<'_>) -> Result<(), String>;

/// A console command of an [`AppHostGame`]: the world and the text after the command word; returns
/// the output lines.
pub type HostConsoleFn = fn(&mut World, &str) -> Vec<String>;

/// Host-event hooks and console commands other crates add to an [`AppHostGame`]'s world
/// (ADR-3354). Adding the same hook or command name twice keeps one copy.
#[derive(Resource, Debug, Clone, Default)]
pub struct HostGameHooks {
    events: Vec<HostEventHook>,
    commands: Vec<(&'static str, &'static str, HostConsoleFn)>,
    rule_sets: Option<HostRuleSetsFn>,
    manifest_rules: bool,
}

/// Where an [`AppHostGame`] reads the game's rule sets from ([`HostGameHooks::set_rule_sets`],
/// ADR-8881): `None` when the world has none.
pub type HostRuleSetsFn = fn(&World) -> Option<regia_rules::ruleset::RuleSetLibrary>;

impl HostGameHooks {
    /// Adds a host-event hook (run in install order).
    pub fn add_event_hook(&mut self, hook: HostEventHook) {
        if !self.events.iter().any(|h| std::ptr::fn_addr_eq(*h, hook)) {
            self.events.push(hook);
        }
    }

    /// Adds a console command: `name` (one word, not a built-in command) and its `help` line.
    pub fn add_command(&mut self, name: &'static str, help: &'static str, run: HostConsoleFn) {
        if !self.commands.iter().any(|(n, _, _)| *n == name) {
            self.commands.push((name, help, run));
        }
    }

    /// The event hooks, in install order.
    pub fn event_hooks(&self) -> &[HostEventHook] {
        &self.events
    }

    /// The help lines of the added commands.
    pub fn command_help(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.commands.iter().map(|(_, help, _)| *help)
    }

    /// The command named `name`.
    pub fn command(&self, name: &str) -> Option<HostConsoleFn> {
        self.commands
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, _, f)| *f)
    }

    /// Sets where the host reads the game's rule sets (ADR-8881; `regia_framework`'s session
    /// bridge sets its `RuleSetRegistry`). The last call wins.
    pub fn set_rule_sets(&mut self, source: HostRuleSetsFn) {
        self.rule_sets = Some(source);
    }

    /// The rule-set source, if one was set.
    pub fn rule_sets(&self) -> Option<HostRuleSetsFn> {
        self.rule_sets
    }

    /// Puts the playlist's rules hash in the session manifest (ADR-8881; default off). Turn it on
    /// only when the game's clients set the same hash (`NetSession::set_rules_hash` with
    /// `RuleSetLibrary::playlist_rules` over the same playlist): the handshake refuses any
    /// client whose manifest differs (`content_mismatch`, `pack:@rules`).
    pub fn set_manifest_rules(&mut self, on: bool) {
        self.manifest_rules = on;
    }

    /// Whether the playlist's rules hash goes into the session manifest.
    pub fn manifest_rules(&self) -> bool {
        self.manifest_rules
    }
}

/// The `[net]` rates of `server.toml` as the host hands them to the game
/// ([`HostGame::set_net_rates`]), or the rates the game's replication sender uses
/// ([`HostGame::net_rates_in_use`]). `None` = not named in `server.toml` (the project's
/// `config/net.ron` value stays), or unknown to the game (ADR-7213).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostNetRates {
    /// `[net] client_rate_bytes`: bytes per second per client.
    pub client_rate_bytes: Option<u32>,
    /// `[net] server_rate_bytes`: total outbound bytes per second (0 = no cap).
    pub server_rate_bytes: Option<u32>,
    /// `[net] send_every_ticks` (at least 1).
    pub send_every_ticks: Option<u32>,
}

/// The replication rates of `config/net.ron` before the host first applied `server.toml`'s, so
/// a key that a reload no longer names goes back to the project's value (ADR-7213).
#[derive(Resource, Debug, Clone, Copy)]
struct NetRonRates {
    client_rate_bytes: u32,
    server_rate_bytes: u32,
    send_every_ticks: u32,
}

/// The game as the host sees it.
pub trait HostGame {
    /// The session.
    fn session(&mut self) -> &mut NetSession;
    /// True when the level is in the server content (pak or dev cache).
    fn level_exists(&mut self, level: &str) -> bool {
        let _ = level;
        true
    }
    /// Loads a level on the server (before the session travels to it).
    fn load_level(&mut self, level: &str) -> Result<(), String> {
        let _ = level;
        Ok(())
    }
    /// One fixed step of the simulation, including the session update.
    fn step(&mut self);
    /// The host paces at `hz` from now on: the game's fixed tick must follow, or every rate
    /// derived from it is wrong (ADR-1892). Called every pacing iteration.
    fn set_tick_hz(&mut self, _hz: u32) {}
    /// The `[net]` rates of `server.toml` (spec 76 § 4): the game's replication sender must use
    /// the named ones, or `server_rate_bytes` has no effect and clients are told a
    /// `send_every_ticks` the server does not send at (ADR-5650). A `None` key is not named: the
    /// game keeps its own (`config/net.ron`) value (ADR-7213). Called at start and after every
    /// reload, before [`Self::net_rates_in_use`].
    fn set_net_rates(&mut self, _rates: &HostNetRates) {}
    /// The rates the game's replication sender uses after [`Self::set_net_rates`]: the host
    /// advertises this `send_every_ticks` in `Welcome` and negotiates this `client_rate_bytes`
    /// (ADR-7213). `None` keys (the default) fall back to `server.toml`'s value, then the built-in
    /// default.
    fn net_rates_in_use(&mut self) -> HostNetRates {
        HostNetRates::default()
    }
    /// `SessionRules::match_over`.
    fn match_over(&mut self) -> bool {
        false
    }
    /// `SessionRules::allow_idle_slowdown`.
    fn allow_idle_slowdown(&mut self) -> bool {
        false
    }
    /// Broadcasts a server chat line; false when the game has no chat handler.
    fn say(&mut self, text: &str) -> bool {
        let _ = text;
        false
    }
    /// Receives the host link (an app game inserts it as a resource for the agent).
    fn install_link(&mut self, link: ServerHostLink) {
        let _ = link;
    }
    /// Binds the game port with the game's own transport (a [`HostedLink`], ADR-2440). `None`
    /// (the default): the game has none, and the session binds the port. `Some(Ok(addr))`: bound;
    /// the session then runs local. `Some(Err((code, message)))`: the bind failed.
    fn host_game_port(&mut self, port: &GamePort) -> Option<Result<SocketAddr, (&'static str, String)>> {
        let _ = port;
        None
    }
    /// The peers on the game's own port, when it hosts one.
    fn game_port_peers(&mut self) -> Option<Vec<PeerId>> {
        None
    }
    /// The server key of the game's own port, when it hosts one.
    fn game_port_key(&mut self) -> Option<ServerKey> {
        None
    }
    /// Removes a peer of the game's own port with `kicked`; false when unknown.
    fn kick_game_peer(&mut self, peer: PeerId) -> bool {
        let _ = peer;
        false
    }
    /// Bans a peer of the game's own port (ADR-2665 (4)): `None` (the default) when the game has
    /// no port of its own.
    fn ban_game_peer(
        &mut self,
        player: &str,
        duration_s: Option<u64>,
        reason: &str,
    ) -> Option<Result<BanId, HostOpError>> {
        let _ = (player, duration_s, reason);
        None
    }
    /// Removes a ban entry and lifts what it refused on the game's own port; `None` (the
    /// default) when the game has no port of its own.
    fn unban_game_entry(&mut self, id: u64) -> Option<Result<(), HostOpError>> {
        let _ = id;
        None
    }
    /// Removes the game-port peers the session's ban list matches (each host tick).
    fn enforce_game_port_bans(&mut self) {}
    /// The protocol id of the game's own link when it may share the game port with the session
    /// (`--shared-port`, ADR-2665 (6)); `None` (the default): it has no link.
    fn game_port_protocol(&mut self) -> Option<u64> {
        None
    }
    /// Hosts the game's link on the link half of a shared game port (ADR-2665 (6)). `None` (the
    /// default): the game has no link; otherwise as [`Self::host_game_port`].
    fn host_game_port_on(
        &mut self,
        port: &GamePort,
        socket: crate::link::LinkSocket,
    ) -> Option<Result<SocketAddr, (&'static str, String)>> {
        let _ = (port, socket);
        None
    }
    /// Tells every peer of the game's own port the server closes, and closes it (drain).
    fn close_game_port(&mut self) {}
    /// Hosts the game's link on the game half of a socket whose relay control traffic the host
    /// reads (`--relay`, ADR-8183). `None` (the default): the game has no link; otherwise as
    /// [`Self::host_game_port`].
    fn host_game_port_relayed(
        &mut self,
        port: &GamePort,
        socket: crate::link::LinkSocket,
    ) -> Option<Result<SocketAddr, (&'static str, String)>> {
        let _ = (port, socket);
        None
    }
    /// Marks a relay's IP on the game's own port (`NetLink::add_relay_ip`). The default does
    /// nothing.
    fn add_game_port_relay_ip(&mut self, ip: std::net::IpAddr) {
        let _ = ip;
    }
    /// The host's relay report, once a second while it has a relay (`net.relay_status`,
    /// ADR-8183). The default does nothing; an app game publishes it into its
    /// `NetAgentState` under the label `server_host`.
    fn publish_relay(&mut self, report: &crate::relay::RelayReport) {
        let _ = report;
    }
    /// The host stopped (after the last tick).
    fn host_stopped(&mut self) {}
    /// A lifecycle point (ADR-3354). The default does nothing.
    fn host_event(&mut self, event: &HostEvent<'_>) -> Result<(), String> {
        let _ = event;
        Ok(())
    }
    /// The game's rule sets (ADR-8881). With a library, the host resolves every playlist row
    /// whose `mode` names one of them: a row that does not resolve is passed over with its
    /// `ruleset_*` code, and the playlist's combined rules hash is computed (shown by
    /// `server.rotation_get`). `None` (the default): rows are not resolved.
    fn rule_sets(&mut self) -> Option<regia_rules::ruleset::RuleSetLibrary> {
        None
    }
    /// True when the playlist's rules hash goes into the session manifest before the session is
    /// hosted (`NetSession::set_rules_hash`), so the handshake refuses a client whose rules
    /// differ. Default `false`: the game's clients must set the same hash first.
    fn rules_in_manifest(&mut self) -> bool {
        false
    }
    /// A console command the host does not know (`cmd`, then the rest of the line): `None` (the
    /// default) when the game has none by that name.
    fn console_command(&mut self, cmd: &str, rest: &str) -> Option<Vec<String>> {
        let _ = (cmd, rest);
        None
    }
    /// Help lines of the game's own console commands.
    fn console_help(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// The lockstep match's status rows when the game is a lockstep server
    /// ([`super::LockstepHostGame`], ADR-8558): the health endpoint's `lockstep` object and the agent's
    /// `server.lockstep_status`. `None` (the default) for every other game.
    fn lockstep_status(&mut self) -> Option<crate::lockstep::server::LockstepServerStatus> {
        None
    }
    /// Catch-up steps after an overrun (spec 76 § 5). The default is [`MAX_SUBSTEPS`] (5).
    /// [`AppHostGame`] reports `FixedTime::max_substeps` instead.
    fn max_substeps(&mut self) -> u32 {
        MAX_SUBSTEPS
    }
}

fn lock<T: ?Sized>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A custom [`HostGame`] that applies `server.toml`'s `[net]` rates itself but leaves
/// [`HostGame::net_rates_in_use`] at the default: `Welcome` then advertises the file's values
/// while the game may send at its own cadence (ADR-5650's bug class, ADR-7213). One warn per
/// named-but-unreported key, at start and at every reload.
fn warn_unreported_net_rates(named: &HostNetRates, in_use: &HostNetRates) {
    for (key, named, reported) in [
        (
            "client_rate_bytes",
            named.client_rate_bytes,
            in_use.client_rate_bytes,
        ),
        (
            "server_rate_bytes",
            named.server_rate_bytes,
            in_use.server_rate_bytes,
        ),
        (
            "send_every_ticks",
            named.send_every_ticks,
            in_use.send_every_ticks,
        ),
    ] {
        if named.is_some() && reported.is_none() {
            tracing::warn!(
                "server host: `[net] {key}` is named, but the game's `net_rates_in_use` reports \
                 None; clients are told server.toml's value — override `net_rates_in_use` to report \
                 what the game's sender uses"
            );
        }
    }
}

/// A server that is a session and nothing else (tools, tests, relays of game messages).
pub struct SessionOnlyGame {
    /// The session.
    pub session: NetSession,
    /// Levels the "pak" holds (empty = every level exists).
    pub levels: Vec<String>,
    /// Game-side match end flag.
    pub match_over: bool,
    /// Game-side idle slowdown opt-in.
    pub idle_slowdown: bool,
    /// Lines broadcast with `say` (no handler when `None`).
    pub chat: Option<Vec<String>>,
    /// Called inside every step (tests advance a [`TestClock`] here).
    pub on_step: Option<StepHook>,
    /// The session clock follows this host clock (milliseconds) when set.
    pub clock: Option<TestClock>,
    /// The tick rate the host paces at.
    pub tick_hz: u32,
}

impl std::fmt::Debug for SessionOnlyGame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOnlyGame")
            .field("levels", &self.levels)
            .finish()
    }
}

impl SessionOnlyGame {
    /// A game around a session.
    pub fn new(session: NetSession) -> Self {
        Self {
            session,
            levels: Vec::new(),
            match_over: false,
            idle_slowdown: false,
            chat: None,
            on_step: None,
            clock: None,
            tick_hz: 0,
        }
    }
}

impl HostGame for SessionOnlyGame {
    fn session(&mut self) -> &mut NetSession {
        &mut self.session
    }

    fn set_tick_hz(&mut self, hz: u32) {
        self.tick_hz = hz;
    }

    fn level_exists(&mut self, level: &str) -> bool {
        self.levels.is_empty() || self.levels.iter().any(|l| l == level)
    }

    fn step(&mut self) {
        if let Some(clock) = &self.clock {
            self.session.set_manual_clock(clock.now().as_millis() as u64);
        }
        if let Some(f) = self.on_step.as_mut() {
            f(&mut self.session);
        }
        self.session.update();
    }

    fn match_over(&mut self) -> bool {
        self.match_over
    }

    fn allow_idle_slowdown(&mut self) -> bool {
        self.idle_slowdown
    }

    fn say(&mut self, text: &str) -> bool {
        match &mut self.chat {
            Some(lines) => {
                lines.push(text.to_owned());
                true
            }
            None => false,
        }
    }
}

/// Level loader of an [`AppHostGame`].
pub type LevelHook = Box<dyn FnMut(&mut App, &str) -> Result<(), String>>;

/// Level presence check of an [`AppHostGame`].
pub type LevelCheck = Box<dyn FnMut(&mut App, &str) -> bool>;

/// A per-step hook of a [`SessionOnlyGame`].
pub type StepHook = Box<dyn FnMut(&mut NetSession) + Send>;

/// Reads one environment variable (tests pass a map).
pub type EnvReader = Box<dyn Fn(&str) -> Option<String> + Send>;

/// A `regia_app::App` as the host's game: `step` is one `App::update` (with
/// [`NetSessionPlugin`](crate::NetSessionPlugin) the session runs in `PreUpdate`); match end and
/// idle slowdown come from the installed [`SessionRules`].
pub struct AppHostGame {
    /// The app (must hold a [`NetSession`] resource).
    pub app: App,
    /// Checks a level is in the server content (default: every level).
    pub level_exists: Option<LevelCheck>,
    /// Loads a level (default: nothing to do).
    pub load_level: Option<LevelHook>,
}

impl AppHostGame {
    /// Wraps an app; inserts an idle [`NetSession`] when it has none.
    pub fn new(mut app: App) -> Self {
        if !app.world().contains_resource::<NetSession>() {
            app.insert_resource(NetSession::default());
        }
        Self {
            app,
            level_exists: None,
            load_level: None,
        }
    }

    fn rules(&self) -> Option<Arc<Mutex<Box<dyn SessionRules>>>> {
        self.app
            .world()
            .get_resource::<NetSession>()
            .map(NetSession::rules)
    }
}

impl HostGame for AppHostGame {
    fn session(&mut self) -> &mut NetSession {
        self.app.world_mut().resource_mut::<NetSession>().into_inner()
    }

    fn level_exists(&mut self, level: &str) -> bool {
        match self.level_exists.as_mut() {
            Some(f) => f(&mut self.app, level),
            None => true,
        }
    }

    fn load_level(&mut self, level: &str) -> Result<(), String> {
        match self.load_level.as_mut() {
            Some(f) => f(&mut self.app, level),
            None => Ok(()),
        }
    }

    fn step(&mut self) {
        self.app.update();
        // A shared game port's session in an app without `NetSessionPlugin` (ADR-2665 (6)).
        if self.app.world().contains_resource::<HostDrivesSession>() {
            crate::session::net_session::net_session_system(self.app.world_mut());
        }
    }

    fn set_net_rates(&mut self, rates: &HostNetRates) {
        let world = self.app.world_mut();
        let Some(settings) = world.get_resource::<crate::settings::NetSettings>() else {
            return;
        };
        let base = match world.get_resource::<NetRonRates>() {
            Some(base) => *base,
            None => {
                let rep = &settings.replication;
                let base = NetRonRates {
                    client_rate_bytes: rep.client_rate_bytes,
                    server_rate_bytes: rep.server_rate_bytes,
                    send_every_ticks: rep.send_every_ticks,
                };
                world.insert_resource(base);
                base
            }
        };
        let mut settings = world.resource_mut::<crate::settings::NetSettings>();
        let rep = &mut settings.replication;
        rep.client_rate_bytes = rates.client_rate_bytes.unwrap_or(base.client_rate_bytes);
        rep.server_rate_bytes = rates.server_rate_bytes.unwrap_or(base.server_rate_bytes);
        rep.send_every_ticks = rates.send_every_ticks.unwrap_or(base.send_every_ticks).max(1);
    }

    fn net_rates_in_use(&mut self) -> HostNetRates {
        self.app
            .world()
            .get_resource::<crate::settings::NetSettings>()
            .map_or_else(HostNetRates::default, |s| HostNetRates {
                client_rate_bytes: Some(s.replication.client_rate_bytes),
                server_rate_bytes: Some(s.replication.server_rate_bytes),
                send_every_ticks: Some(s.replication.send_every_ticks.max(1)),
            })
    }

    fn set_tick_hz(&mut self, hz: u32) {
        let world = self.app.world_mut();
        let Some(mut fixed) = world.get_resource_mut::<regia_app::FixedTime>() else {
            return;
        };
        let hz = f64::from(hz.max(1));
        if fixed.hz == hz {
            return;
        }
        fixed.hz = hz;
        // A fixed-step app feeds one step per frame: the frame delta follows the new step (idle
        // slowdown must slow the simulation clock with the pacing, not leave a 1/60 s delta
        // against a 0.1 s step), and time banked for the old step is dropped.
        fixed.clear_accumulator();
        let step = fixed.step();
        if let Some(mut strategy) = world.get_resource_mut::<regia_app::TimeUpdateStrategy>()
            && matches!(*strategy, regia_app::TimeUpdateStrategy::ManualDuration(_))
        {
            *strategy = regia_app::TimeUpdateStrategy::ManualDuration(step);
        }
    }

    fn max_substeps(&mut self) -> u32 {
        self.app
            .world()
            .get_resource::<regia_app::FixedTime>()
            .map(|fixed| fixed.max_substeps)
            .unwrap_or(MAX_SUBSTEPS)
            .clamp(1, regia_app::MAX_SUBSTEPS_LIMIT)
    }

    fn install_link(&mut self, link: ServerHostLink) {
        self.app.insert_resource(link);
    }

    fn publish_relay(&mut self, report: &crate::relay::RelayReport) {
        let world = self.app.world_mut();
        if !world.contains_resource::<crate::NetAgentState>() {
            world.insert_resource(crate::NetAgentState::default());
        }
        world
            .resource_mut::<crate::NetAgentState>()
            .publish_relay("server_host", report.clone());
    }

    fn host_game_port(&mut self, port: &GamePort) -> Option<Result<SocketAddr, (&'static str, String)>> {
        let hosted = self.app.world().get_resource::<HostedLink>()?.clone();
        let world = self.app.world_mut();
        if !world.contains_resource::<crate::NetLink>() {
            return Some(Err((
                "link_missing",
                "the app has a HostedLink but no NetLink (add NetLinkPlugin)".to_owned(),
            )));
        }
        let mut link = world.resource_mut::<crate::NetLink>();
        Some(
            hosted
                .bind(&mut link, port)
                .map_err(|e| (e.code(), e.to_string())),
        )
    }

    fn game_port_protocol(&mut self) -> Option<u64> {
        let world = self.app.world();
        world.get_resource::<crate::NetLink>()?;
        world.get_resource::<HostedLink>().map(HostedLink::protocol)
    }

    fn host_game_port_relayed(
        &mut self,
        port: &GamePort,
        socket: crate::link::LinkSocket,
    ) -> Option<Result<SocketAddr, (&'static str, String)>> {
        let hosted = self.app.world().get_resource::<HostedLink>()?.clone();
        let mut link = self.app.world_mut().get_resource_mut::<crate::NetLink>()?;
        Some(
            hosted
                .bind_on(&mut link, socket, port)
                .map_err(|e| (e.code(), e.to_string())),
        )
    }

    fn add_game_port_relay_ip(&mut self, ip: std::net::IpAddr) {
        if let Some(mut link) = self.app.world_mut().get_resource_mut::<crate::NetLink>() {
            link.add_relay_ip(ip);
        }
    }

    fn host_game_port_on(
        &mut self,
        port: &GamePort,
        socket: crate::link::LinkSocket,
    ) -> Option<Result<SocketAddr, (&'static str, String)>> {
        let hosted = self.app.world().get_resource::<HostedLink>()?.clone();
        let bound = {
            let mut link = self.app.world_mut().get_resource_mut::<crate::NetLink>()?;
            hosted
                .bind_on(&mut link, socket, port)
                .map_err(|e| (e.code(), e.to_string()))
        };
        // The session on the other half needs a driver: the app's `NetSessionPlugin`, else the
        // host's step.
        if bound.is_ok()
            && !self
                .app
                .has_plugin(std::any::type_name::<crate::NetSessionPlugin>())
        {
            self.app.insert_resource(HostDrivesSession);
        }
        Some(bound)
    }

    fn game_port_peers(&mut self) -> Option<Vec<PeerId>> {
        let world = self.app.world();
        world.get_resource::<HostedLink>()?;
        let link = world.get_resource::<crate::NetLink>()?;
        if link.role() != "server" {
            return Some(Vec::new());
        }
        Some(
            world
                .get_resource::<crate::runtime::NetNode>()
                .map(crate::runtime::NetNode::peers)
                .unwrap_or_default(),
        )
    }

    fn game_port_key(&mut self) -> Option<ServerKey> {
        let world = self.app.world();
        world.get_resource::<HostedLink>()?;
        world.get_resource::<crate::NetLink>()?.server_key()
    }

    fn kick_game_peer(&mut self, peer: PeerId) -> bool {
        self.app
            .world_mut()
            .get_resource_mut::<crate::NetLink>()
            .is_some_and(|mut link| link.kick(peer))
    }

    fn ban_game_peer(
        &mut self,
        player: &str,
        duration_s: Option<u64>,
        reason: &str,
    ) -> Option<Result<BanId, HostOpError>> {
        let world = self.app.world_mut();
        link_hosts_game_port(world).then(|| ban_link_peer(world, player, duration_s, reason))
    }

    fn unban_game_entry(&mut self, id: u64) -> Option<Result<(), HostOpError>> {
        let world = self.app.world_mut();
        link_hosts_game_port(world).then(|| unban_link_entry(world, id))
    }

    fn enforce_game_port_bans(&mut self) {
        enforce_link_bans(self.app.world_mut());
    }

    fn close_game_port(&mut self) {
        if !self.app.world().contains_resource::<HostedLink>() {
            return;
        }
        let status = self
            .app
            .world()
            .get_resource::<crate::NetLink>()
            .and_then(crate::NetLink::status);
        if let Some(mut hosted) = self.app.world_mut().get_resource_mut::<HostedLink>() {
            hosted.set_final_status(status);
        }
        if let Some(mut link) = self.app.world_mut().get_resource_mut::<crate::NetLink>() {
            link.close();
        }
    }

    fn host_stopped(&mut self) {
        let hook = self
            .app
            .world()
            .get_resource::<HostedLink>()
            .and_then(HostedLink::on_stop);
        if let Some(hook) = hook {
            hook(self.app.world_mut());
        }
    }

    fn rule_sets(&mut self) -> Option<regia_rules::ruleset::RuleSetLibrary> {
        let world = self.app.world();
        let source = world.get_resource::<HostGameHooks>()?.rule_sets()?;
        source(world)
    }

    fn rules_in_manifest(&mut self) -> bool {
        self.app
            .world()
            .get_resource::<HostGameHooks>()
            .is_some_and(HostGameHooks::manifest_rules)
    }

    fn host_event(&mut self, event: &HostEvent<'_>) -> Result<(), String> {
        let hooks: Vec<HostEventHook> = self
            .app
            .world()
            .get_resource::<HostGameHooks>()
            .map(|h| h.event_hooks().to_vec())
            .unwrap_or_default();
        let mut first_err = None;
        for hook in hooks {
            if let Err(e) = hook(self.app.world_mut(), event) {
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn console_command(&mut self, cmd: &str, rest: &str) -> Option<Vec<String>> {
        let run = self.app.world().get_resource::<HostGameHooks>()?.command(cmd)?;
        Some(run(self.app.world_mut(), rest))
    }

    fn console_help(&mut self) -> Vec<String> {
        self.app
            .world()
            .get_resource::<HostGameHooks>()
            .map(|h| h.command_help().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn match_over(&mut self) -> bool {
        self.rules()
            .is_some_and(|r| lock(&r).match_over(self.app.world()))
    }

    fn allow_idle_slowdown(&mut self) -> bool {
        self.rules().is_some_and(|r| lock(&r).allow_idle_slowdown())
    }
}

/// Marks an app whose session the host updates each step: a shared game port (ADR-2665 (6)) in
/// an app without `NetSessionPlugin`.
#[derive(Resource, Debug, Default, Clone, Copy)]
struct HostDrivesSession;

/// Tick timing over the last minute.
#[derive(Debug, Default, Clone)]
pub struct TickStats {
    window: VecDeque<(Duration, f64)>,
    overruns: VecDeque<Duration>,
    /// Overruns since start.
    pub overruns_total: u64,
    /// Worst tick since start, milliseconds.
    pub worst_ms: f64,
}

impl TickStats {
    fn record(&mut self, at: Duration, ms: f64, overrun: bool) {
        self.window.push_back((at, ms));
        if overrun {
            self.overruns.push_back(at);
            self.overruns_total += 1;
        }
        self.worst_ms = self.worst_ms.max(ms);
        let cutoff = at.saturating_sub(Duration::from_secs(60));
        while self.window.front().is_some_and(|(t, _)| *t < cutoff) {
            self.window.pop_front();
        }
        while self.overruns.front().is_some_and(|t| *t < cutoff) {
            self.overruns.pop_front();
        }
    }

    /// Overruns in the last minute.
    pub fn overruns_last_min(&self) -> usize {
        self.overruns.len()
    }

    /// A percentile (0..=100) of tick time over the last minute, milliseconds.
    pub fn percentile_ms(&self, p: f64) -> f64 {
        if self.window.is_empty() {
            return 0.0;
        }
        let mut v: Vec<f64> = self.window.iter().map(|(_, ms)| *ms).collect();
        v.sort_by(f64::total_cmp);
        let i = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
        v[i.min(v.len() - 1)]
    }
}

/// What a reload applied and what needs a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadReport {
    /// Keys applied.
    pub applied: Vec<String>,
    /// Keys that changed but need a restart.
    pub needs_restart: Vec<String>,
}

/// How the host exposes the game port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostTransport {
    /// UDP on `server.bind:server.port`.
    Udp,
    /// UDP on `server.bind` with a port the OS picks (tests, dev runs: nothing waits on a fixed
    /// port); read it back with [`ServerHost::game_addr`]. `server.port` is ignored.
    UdpAnyPort,
    /// No network: loopback and simulated clients only (tests, in-process play).
    Local,
}

/// Whether the host maps its game port for a registration with `relay`: `Ok` to map, or the code
/// the host check reports instead: `port_map_off` for a relay on this machine (no NAT between),
/// `port_map_disabled` for `--no-port-map` (N-19, ADR-10069).
fn port_map_choice(relay: SocketAddr, cli: &CliOverrides) -> Result<(), &'static str> {
    if relay.ip().is_loopback() {
        Err("port_map_off")
    } else if cli.no_port_map {
        Err("port_map_disabled")
    } else {
        Ok(())
    }
}

/// Default drain grace, seconds.
pub const DEFAULT_DRAIN_GRACE_S: f64 = 5.0;
/// Default catch-up steps when the game does not expose a cap (`FixedTime::max_substeps` is 5).
pub const MAX_SUBSTEPS: u32 = 5;

/// The dedicated-server host.
pub struct ServerHost {
    settings: ServerSettings,
    base_dir: PathBuf,
    cli: CliOverrides,
    env: EnvReader,
    config_warnings: Vec<String>,
    state: HostState,
    clock: Box<dyn HostClock>,
    signals: HostSignals,
    console_tx: Sender<String>,
    console_rx: Receiver<String>,
    console_out: Vec<String>,
    health: Option<HealthServer>,
    tick: u64,
    next_tick: Duration,
    started: Duration,
    stats: TickStats,
    /// The `regia_net::stats` operator rows, on for a dedicated server (spec 120 § 13.2,
    /// ADR-8182).
    net_stats: crate::stats::StatsReporter,
    rotation_index: usize,
    /// Live rows. `index` is the next row [`Self::next_level`] takes.
    playlist: regia_rules::playlist::Playlist,
    /// The last row `next_level` could not load.
    last_failure: Option<RotationFailure>,
    /// Rows the last `next_level` passed over (ADR-8897).
    rotation_skips: Vec<RotationSkip>,
    /// The game's rule sets, read at start (ADR-8881); `None` when the game has none.
    rule_sets: Option<regia_rules::ruleset::RuleSetLibrary>,
    /// The playlist's combined rules hash (ADR-8881).
    rules_hash: Option<u64>,
    /// True when the session manifest carries `rules_hash`.
    rules_in_manifest: bool,
    level: String,
    /// The session travel id of an in-game travel whose level failed to load on the host
    /// (finding 5, deep-zai net-lobby): the host keeps running the old level while the session
    /// reports the new one, so that travel is neither retried nor re-reported every tick. Keyed
    /// by the travel id, not the level name, so a new travel to the same level (fixed content, a
    /// second `server.change_level`) tries again. Cleared by the next successful travel.
    travel_failed: Option<u32>,
    match_end_at: Option<Duration>,
    drain_until: Option<Duration>,
    drain_ticks: u32,
    shutdown_requests: u32,
    last_minute_log: Duration,
    last_health: Option<Duration>,
    /// Allows remote admin on an unencrypted transport (dev builds only; tests and LAN dev).
    pub allow_unencrypted_admin: bool,
    remote_admin: bool,
    exit: Option<ExitCode>,
    link: ServerHostLink,
    game_port: Option<SocketAddr>,
    game_owns_port: bool,
    /// The game port shared by the link and the session (`--shared-port`, ADR-2665 (6)).
    shared_port: Option<SharedPort>,
    registrar: Option<RelayRegistrar>,
    /// The game socket's port mapping (spec 120 § 10.7), raced and renewed on its own thread.
    port_map: Option<crate::port_map::PortMapWorker>,
    /// The host check's result was logged.
    host_check_logged: bool,
    local_relay: Option<LocalRelay>,
    next_relay_report_ms: u64,
    /// The store's join ticket hook [`Self::start`] hands the game's session (KI-NT-047).
    ticket_check: Option<std::sync::Arc<dyn crate::ticket::JoinTicketCheck>>,
}

impl std::fmt::Debug for ServerHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerHost")
            .field("state", &self.state)
            .field("tick", &self.tick)
            .field("level", &self.level)
            .finish()
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Parses a ban duration: `30m`, `12h`, `7d`, `90s` (seconds).
pub fn parse_duration_s(text: &str) -> Option<u64> {
    let text = text.trim();
    let (cut, _) = text.char_indices().last()?;
    let (num, unit) = text.split_at(cut);
    let n: u64 = num.parse().ok()?;
    let mul = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    n.checked_mul(mul)
}

/// Parses a console player id: `peer.local` (one player) or `peer` (every player of the peer).
pub fn parse_player_id(text: &str) -> Option<(PeerId, Option<u8>)> {
    match text.split_once('.') {
        Some((p, l)) => Some((PeerId(p.parse().ok()?), Some(l.parse().ok()?))),
        None => Some((PeerId(text.parse().ok()?), None)),
    }
}

/// A failed host operation: a stable lowercase code (the agent's `data.reason`) and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOpError {
    /// Stable code (`net_not_server`, `player_unknown`, `ban_entry_unknown`, ...).
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl std::fmt::Display for HostOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub(crate) fn op_err(code: &'static str, message: impl Into<String>) -> HostOpError {
    HostOpError {
        code,
        message: message.into(),
    }
}

pub(crate) fn resolve_target(session: &NetSession, text: &str) -> Result<(PeerId, Option<u8>), HostOpError> {
    let (peer, local) = parse_player_id(text)
        .ok_or_else(|| op_err("player_unknown", format!("bad player id `{text}` (peer.local)")))?;
    let server = session
        .server()
        .ok_or_else(|| op_err("net_not_server", "this process is not a server"))?;
    let known = server.peer_state(peer).is_some()
        && local.is_none_or(|l| server.player_name(NetPlayerId { peer, local: l }).is_some());
    if known {
        Ok((peer, local))
    } else {
        Err(op_err("player_unknown", text.to_owned()))
    }
}

/// Kicks one player (`peer.local`) or every player of a peer (`peer`).
pub fn kick_player(session: &mut NetSession, player: &str, reason: &str) -> Result<(), HostOpError> {
    let (peer, local) = resolve_target(session, player)?;
    let done = match local {
        Some(local) => session.kick(NetPlayerId { peer, local }, reason),
        None => session.kick_peer(peer, reason),
    };
    done.map_err(|e| op_err(e.code(), e.to_string()))?;
    tracing::info!(%player, %reason, "server host: kick");
    Ok(())
}

/// Reads the admin token from `path`, refusing a file other users can access (spec 76 § 7):
/// mode bits for group/other on Unix; a DACL grant to any principal other than the owner,
/// SYSTEM and Administrators on Windows (any access, not only read: a writable token can be
/// replaced with a known one).
fn read_admin_token_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !super::platform::admin_token_file_private(path, &meta) {
        return None;
    }
    let token = std::fs::read_to_string(path).ok()?.trim().to_owned();
    if token.is_empty() {
        return None;
    }
    Some(token)
}

/// Bans a player's platform id (when the authenticator gave one, else its address block) for
/// `duration_s` (permanent when `None`) and kicks the peer. An IPv6 address is written with an
/// explicit `/64`: the ban is a block ban, and the explicit spelling keeps it one in `bans.ron`
/// (ADR-4936). The expiry is `duration_s` on the session clock ([`NetSession::now_unix_s`]), the
/// clock the session's join check reads, never the system clock (ADR-5973).
pub fn ban_player(
    session: &mut NetSession,
    player: &str,
    duration_s: Option<u64>,
    reason: &str,
) -> Result<BanId, HostOpError> {
    let (peer, _) = resolve_target(session, player)?;
    let server = session
        .server()
        .ok_or_else(|| op_err("net_not_server", "this process is not a server"))?;
    let (kind, value) = match server.peer_platform_id(peer) {
        Some(pid) => (BanKind::PlatformId, pid.to_owned()),
        None => {
            let ip = server.peer_addr(peer).map(|a| a.ip().to_canonical());
            let value = match ip {
                Some(std::net::IpAddr::V4(v4)) => v4.to_string(),
                Some(std::net::IpAddr::V6(v6)) => format!("{v6}/64"),
                None => String::new(),
            };
            (BanKind::Address, value)
        }
    };
    // The ban list's `until` is on the session clock: the join check compares it with that clock,
    // so a ban issued for 60 s ends after 60 s even when the system clock steps (ADR-5973).
    let until = duration_s.map(|d| session.now_unix_s().saturating_add(d));
    let id = match session.ban(kind, &value, until, reason) {
        Ok(id) => id,
        Err(crate::session::SessionError::BanStoreSaveFailed(e)) => {
            // The save failed but the entry is in memory for this run (a locked `bans.ron`, a
            // read-only store): the peer must still leave, and the operator gets the save error
            // (audit net1: previously the error skipped the kick and the abusive peer stayed).
            let _ = session.kick_peer(peer, &format!("banned: {reason}"));
            tracing::info!(%player, kind = kind.as_str(), %value, ?until, "server host: ban (in memory only)");
            return Err(op_err("ban_store_save_failed", e));
        }
        Err(e) => return Err(op_err(e.code(), e.to_string())),
    };
    let _ = session.kick_peer(peer, &format!("banned: {reason}"));
    tracing::info!(%player, kind = kind.as_str(), %value, ?until, "server host: ban");
    Ok(id)
}

/// Which layer of the game port `player` names (see [`player_layer`]).
fn game_port_player<'a>(game: &mut dyn HostGame, player: &'a str) -> Result<(PlayerLayer, &'a str), String> {
    let peers = game.game_port_peers().unwrap_or_default();
    player_layer(player, &peers, Some(&*game.session())).map_err(|e| e.to_string())
}

/// Removes a ban entry. The save that follows drops entries expired on the session clock, the
/// clock their `until` is stamped on (ADR-5973).
pub fn unban_entry(session: &mut NetSession, id: u64) -> Result<(), HostOpError> {
    let now = session.now_unix_s();
    let server = session
        .server_mut()
        .ok_or_else(|| op_err("net_not_server", "this process is not a server"))?;
    match server.bans_mut().remove(BanId(id), now) {
        Ok(true) => Ok(()),
        Ok(false) => Err(op_err("ban_entry_unknown", format!("no ban entry {id}"))),
        Err(e) => Err(op_err("ban_store_save_failed", e.to_string())),
    }
}

/// One playlist row as the agent reads it (`server.rotation_get`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRowView {
    /// The level path.
    pub map_id: String,
    /// Mode id, or empty for the level's own rules.
    pub mode: String,
    /// Row options in key order.
    pub options: Vec<(String, String)>,
}

/// The last row `next_level` could not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationFailure {
    /// The level path.
    pub map_id: String,
    /// The row's mode id.
    pub mode: String,
    /// Why the row was skipped.
    pub error: String,
}

/// A row the last travel passed over (`server.rotation_get`, ADR-8897).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationSkip {
    /// The row's playlist index.
    pub index: u32,
    /// The level path.
    pub map_id: String,
    /// The row's mode id.
    pub mode: String,
    /// Why: `too_few_players`, `too_many_players`, `missing_content` (the level is not in the
    /// pak), `load_failed`, or the `ruleset_*` code of a row whose rule set does not resolve
    /// (ADR-8881).
    pub reason: String,
}

/// The live rotation (`server.rotation_get`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RotationView {
    /// Every row, in play order.
    pub rows: Vec<RotationRowView>,
    /// The row the host is on.
    pub index: u32,
    /// Start over after the last row.
    pub looping: bool,
    /// The last row that failed to load, if any.
    pub last_failure: Option<RotationFailure>,
    /// The playlist order: `in_order`, `shuffle` or `weighted` (ADR-8897).
    pub order: String,
    /// Rows the last travel passed over, in walk order (ADR-8897).
    pub skipped: Vec<RotationSkip>,
    /// The playlist's combined rules hash (ADR-8881), when the game gave the host its rule sets
    /// ([`HostGame::rule_sets`]) and a row names one of them.
    pub rules_hash: Option<u64>,
    /// True when the session manifest carries `rules_hash` ([`HostGame::rules_in_manifest`]).
    pub rules_in_manifest: bool,
}

/// What the host shares with agent methods running inside the game's world.
#[derive(Debug, Clone)]
pub struct HostShared {
    /// The last `/health` JSON (refreshed once a second and on every state change).
    pub health_json: String,
    /// The host state.
    pub state: HostState,
    /// The resolved settings.
    pub settings: ServerSettings,
    /// The server directory.
    pub base_dir: PathBuf,
    /// The command-line overrides (for `server.reload_config`'s preview).
    pub cli: CliOverrides,
    /// The drain grace the next shutdown request uses (`server.shutdown {grace_s}`).
    pub drain_grace_s: Option<f64>,
    /// The live rotation (rows, current index, looping, last failure).
    pub rotation: RotationView,
}

/// A resource the host inserts into an [`AppHostGame`]'s world: the request flags and the shared
/// host state, so the agent's `server.*` methods reach the host.
#[derive(Resource, Debug, Clone)]
pub struct ServerHostLink {
    /// The host's request flags.
    pub signals: HostSignals,
    /// Shared state.
    pub shared: Arc<Mutex<HostShared>>,
}

impl ServerHostLink {
    /// A copy of the shared state.
    pub fn snapshot(&self) -> HostShared {
        lock(&self.shared).clone()
    }
}

/// The console commands (spec 76 § 6).
pub const CONSOLE_HELP: &[&str] = &[
    "status                          name, level, tick, players, overruns",
    "players                         one line per player",
    "kick <player-id> [reason]       disconnect with `kicked`",
    "ban <player-id> [duration] [reason]  ban (30m, 12h, 7d; default permanent) and kick",
    "unban <entry-id>                remove a ban entry",
    "bans                            list ban entries",
    "level <path>                    travel to a level now",
    "next                            travel to the next level in the rotation",
    "say <text>                      broadcast a server chat line",
    "reload                          re-read server.toml and bans.ron",
    "serverkey                       the public key clients pin",
    "joincode                        the join code (with --relay)",
    "revokecode                      withdraw the join code on the relay (joined players stay)",
    "relay                           the local relay's allocations and counters (with --relay local)",
    "quit [grace-seconds]            drain and stop (default 5 s)",
    "help                            this list",
];

/// Loads the live playlist: a `.playlist.ron` when `levels.playlist` is set, otherwise one
/// map-only looping row per `levels.rotation` entry (`mode` empty: the level's own rules).
fn load_playlist(base: &Path, settings: &ServerSettings) -> Result<regia_rules::playlist::Playlist, String> {
    use regia_rules::playlist::{Playlist, PlaylistEntry};
    if settings.playlist.is_empty() {
        return Ok(Playlist {
            entries: settings
                .rotation
                .iter()
                .map(|map_id| PlaylistEntry::new(map_id, ""))
                .collect(),
            index: 0,
            looping: true,
            ..Playlist::default()
        });
    }
    let path = base.join(&settings.playlist);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("levels.playlist: {e}"))?;
    let file = crate::formats::PlaylistFile::from_ron(&text).map_err(|e| format!("levels.playlist: {e}"))?;
    // The `regia.playlist` id is the file name without `.playlist.ron` (ADR-8897).
    let id = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.strip_suffix(".playlist.ron").unwrap_or(name).to_owned())
        .unwrap_or_default();
    Ok(Playlist {
        entries: file.entries,
        index: 0,
        looping: file.looping,
        order: file.order,
        no_repeat_last: file.no_repeat_last,
        id,
        ..Playlist::default()
    })
}

/// The error of `row`'s rule set when `rule_sets` is set and the row's mode names a rule set that
/// does not resolve with the row's options (ADR-8881,
/// [`regia_rules::ruleset::RuleSetLibrary::resolve_hosted_row`]).
fn rule_error(
    rule_sets: Option<&regia_rules::ruleset::RuleSetLibrary>,
    row: &regia_rules::playlist::PlaylistEntry,
) -> Option<regia_rules::ruleset::RuleSetError> {
    rule_sets?.resolve_hosted_row(row)?.err()
}

fn rotation_view_of(
    playlist: &regia_rules::playlist::Playlist,
    index: usize,
    last: &Option<RotationFailure>,
    skipped: &[RotationSkip],
    rules_hash: Option<u64>,
    rules_in_manifest: bool,
) -> RotationView {
    RotationView {
        rows: playlist
            .entries
            .iter()
            .map(|row| RotationRowView {
                map_id: row.map_id.clone(),
                mode: row.mode.clone(),
                options: row.options.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            })
            .collect(),
        index: u32::try_from(index).unwrap_or(u32::MAX),
        looping: playlist.looping,
        last_failure: last.clone(),
        order: playlist.order.as_str().to_owned(),
        skipped: skipped.to_vec(),
        rules_hash,
        rules_in_manifest,
    }
}

impl ServerHost {
    /// Resolves the configuration (`base_dir` holds `server.toml`, `bans.ron`, `content/`).
    /// A configuration error is exit code 2.
    pub fn new(
        base_dir: &Path,
        cli: CliOverrides,
        env: EnvReader,
        clock: Box<dyn HostClock>,
    ) -> Result<Self, HostError> {
        let resolved = resolve_config(base_dir, &cli, &*env).map_err(|errors| HostError {
            exit: ExitCode::Config,
            lines: errors.iter().map(ToString::to_string).collect(),
        })?;
        for w in &resolved.warnings {
            tracing::warn!("{w}");
        }
        let mut playlist = load_playlist(base_dir, &resolved.settings).map_err(|line| HostError {
            exit: ExitCode::Config,
            lines: vec![line],
        })?;
        if resolved.settings.start_index >= playlist.entries.len() {
            return Err(HostError::new(
                ExitCode::Config,
                format!("levels.start_index: 0..{}", playlist.entries.len()),
            ));
        }
        let rotation_index = resolved.settings.start_index;
        // The host is already on `start_index`: in order, the next travel is the following row;
        // shuffled or weighted, the repeat guard knows the row (ADR-8897).
        playlist.start_at(u32::try_from(rotation_index).unwrap_or(u32::MAX));
        let (console_tx, console_rx) = channel();
        let now = clock.now();
        let signals = HostSignals::new();
        let link = ServerHostLink {
            signals: signals.clone(),
            shared: Arc::new(Mutex::new(HostShared {
                health_json: String::new(),
                state: HostState::Starting,
                settings: resolved.settings.clone(),
                base_dir: base_dir.to_owned(),
                cli: cli.clone(),
                drain_grace_s: None,
                rotation: rotation_view_of(&playlist, rotation_index, &None, &[], None, false),
            })),
        };
        Ok(Self {
            settings: resolved.settings,
            base_dir: base_dir.to_owned(),
            cli,
            env,
            config_warnings: resolved.warnings,
            state: HostState::Starting,
            clock,
            signals,
            console_tx,
            console_rx,
            console_out: Vec::new(),
            health: None,
            tick: 0,
            next_tick: now,
            started: now,
            stats: TickStats::default(),
            net_stats: crate::stats::StatsReporter::new(crate::stats::StatsConfig::dedicated_server()),
            rotation_index,
            playlist,
            last_failure: None,
            rotation_skips: Vec::new(),
            rule_sets: None,
            rules_hash: None,
            rules_in_manifest: false,
            level: String::new(),
            travel_failed: None,
            match_end_at: None,
            drain_until: None,
            drain_ticks: 0,
            shutdown_requests: 0,
            last_minute_log: now,
            last_health: None,
            allow_unencrypted_admin: false,
            remote_admin: false,
            exit: None,
            link,
            game_port: None,
            game_owns_port: false,
            shared_port: None,
            registrar: None,
            port_map: None,
            host_check_logged: false,
            local_relay: None,
            next_relay_report_ms: 0,
            ticket_check: None,
        })
    }

    /// The resolved settings.
    pub fn settings(&self) -> &ServerSettings {
        &self.settings
    }

    /// Warnings from the configuration (unknown keys).
    pub fn config_warnings(&self) -> &[String] {
        &self.config_warnings
    }

    /// The state.
    pub fn state(&self) -> HostState {
        self.state
    }

    /// Ticks run.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The current level.
    pub fn level(&self) -> &str {
        &self.level
    }

    /// Tick timing.
    pub fn stats(&self) -> &TickStats {
        &self.stats
    }

    /// The request flags (shared with the OS handlers).
    pub fn signals(&self) -> &HostSignals {
        &self.signals
    }

    /// The link the host gives the game (agent methods).
    pub fn link(&self) -> &ServerHostLink {
        &self.link
    }

    /// True when remote admin is on.
    pub fn remote_admin(&self) -> bool {
        self.remote_admin
    }

    /// The game port shared by the link and the session, when `--shared-port` is on and the
    /// game has a link (ADR-2665 (6)).
    pub fn shared_port(&self) -> Option<&SharedPort> {
        self.shared_port.as_ref()
    }

    /// The bound game port (the session's, or the game's own [`HostedLink`]).
    pub fn game_addr(&self) -> Option<SocketAddr> {
        self.game_port
    }

    /// True when the game's own transport binds the game port (ADR-2440).
    pub fn game_owns_port(&self) -> bool {
        self.game_owns_port
    }

    /// The join code, once the relay gave one (`--relay`).
    pub fn join_code(&self) -> Option<&str> {
        self.registrar.as_ref().and_then(RelayRegistrar::join_code)
    }

    /// Withdraws the join code on the relay (N-19, ADR-10069; console `revokecode`): see
    /// [`RelayRegistrar::revoke_code`]. False without `--relay`. Players already joined stay; a
    /// later join by the code is refused once the relay confirmed ([`Self::join_code_revoked`]).
    pub fn revoke_join_code(&mut self) -> bool {
        let now_ms = self.clock.now().as_millis() as u64;
        match self.registrar.as_mut() {
            Some(registrar) => {
                registrar.revoke_code(now_ms);
                true
            }
            None => false,
        }
    }

    /// True once the relay confirmed [`Self::revoke_join_code`].
    pub fn join_code_revoked(&self) -> bool {
        self.registrar.as_ref().is_some_and(RelayRegistrar::code_revoked)
    }

    /// The local relay's state (`--relay local`); `None` for a remote relay or none.
    pub fn relay_status(&self) -> Option<crate::relay::RelayStatus> {
        self.local_relay.as_ref().map(LocalRelay::status)
    }

    /// The relay the host registers with (`--relay`), the local one's address included.
    pub fn relay_addr(&self) -> Option<SocketAddr> {
        self.registrar.as_ref().map(RelayRegistrar::relay)
    }

    /// The health endpoint's bound address.
    pub fn health_addr(&self) -> Option<std::net::SocketAddr> {
        self.health.as_ref().map(HealthServer::addr)
    }

    /// Sends console lines in (a stdin reader thread, or tests).
    pub fn console_sender(&self) -> Sender<String> {
        self.console_tx.clone()
    }

    /// Takes console output lines.
    pub fn take_console_output(&mut self) -> Vec<String> {
        std::mem::take(&mut self.console_out)
    }

    /// Starts a thread reading stdin lines into the console (when stdin is a terminal, or
    /// `--console`).
    pub fn spawn_stdin_console(&self) {
        use std::io::{BufRead, IsTerminal};
        if !(self.settings.console || std::io::stdin().is_terminal()) {
            return;
        }
        let tx = self.console_tx.clone();
        let _ = std::thread::Builder::new()
            .name("regia-console".into())
            .spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
    }

    /// The session options. `in_use` is what the game's sender uses
    /// ([`HostGame::net_rates_in_use`]); clients are told that cadence and negotiate that rate.
    fn host_options(&self, token: Option<String>, in_use: &HostNetRates) -> HostOptions {
        let defaults = HostOptions::default();
        HostOptions {
            name: self.settings.name.clone(),
            motd: self.settings.motd.clone(),
            max_players: self.settings.max_players,
            password: self.settings.password.clone(),
            tick_hz: self.settings.tick_hz,
            send_every_ticks: in_use
                .send_every_ticks
                .or(self.settings.send_every_ticks)
                .unwrap_or(defaults.send_every_ticks)
                .max(1),
            level: self.level.clone(),
            client_rate_bytes: in_use
                .client_rate_bytes
                .or(self.settings.client_rate_bytes)
                .unwrap_or(defaults.client_rate_bytes),
            entropy: self.settings.entropy,
            admin_token: token,
            allow_plain_admin: self.dev_plain_admin(),
        }
    }

    fn net_rates(&self) -> HostNetRates {
        HostNetRates {
            client_rate_bytes: self.settings.client_rate_bytes,
            server_rate_bytes: self.settings.server_rate_bytes,
            send_every_ticks: self.settings.send_every_ticks.map(|n| n.max(1)),
        }
    }

    fn dev_plain_admin(&self) -> bool {
        self.allow_unencrypted_admin && cfg!(debug_assertions)
    }

    /// Loads or creates the server's key (spec 76 § 7, ADR-1901). A key file that cannot be read
    /// stops the start: a fresh key would quietly break every client's pin.
    fn load_identity(&self) -> Result<ServerIdentity, HostError> {
        let path = self.base_dir.join(&self.settings.identity_file);
        let identity = ServerIdentity::load_or_create(&path)
            .map_err(|e| HostError::new(ExitCode::Config, format!("net.identity_file: {e}")))?;
        tracing::info!(server_key = %identity.public(), "server host: identity loaded");
        Ok(identity)
    }

    fn read_admin_token(&self, transport: HostTransport) -> Option<String> {
        if self.settings.admin_token_file.is_empty() {
            return None;
        }
        let token = read_admin_token_file(&self.base_dir.join(&self.settings.admin_token_file))?;
        let encrypted = transport != HostTransport::Local && self.settings.encryption.can_encrypt();
        if !encrypted && !self.dev_plain_admin() {
            tracing::warn!("server host: remote admin needs the encrypted transport; off");
            return None;
        }
        Some(token)
    }

    /// Installs the store's join ticket hook on the dedicated server (KI-NT-047, ADR-10069):
    /// [`Self::start`] hands it to the game's session
    /// ([`crate::session::NetSession::set_join_ticket_check`]), so a client's
    /// `ConnectOptions::auth` ticket is checked after the password, a late store refusal kicks
    /// the player with `ticket_refused`, and the store session ends when the player is gone. Call
    /// it before `start`; a game may also install the hook on its session itself.
    pub fn set_join_ticket_check(&mut self, check: std::sync::Arc<dyn crate::ticket::JoinTicketCheck>) {
        self.ticket_check = Some(check);
    }

    /// `Starting` → `Loading` → `Running`: logs, manifest, bans, port, first level, health.
    pub fn start(&mut self, game: &mut dyn HostGame, transport: HostTransport) -> Result<(), HostError> {
        tracing::info!(phase = "startup", name = %self.settings.name, "server host: starting");
        if let Some(check) = &self.ticket_check {
            game.session().set_join_ticket_check(Some(check.clone()));
        }
        let log_dir = self.base_dir.join(&self.settings.log_dir);
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            tracing::warn!(dir = %log_dir.display(), error = %e, "server host: cannot create the log directory");
        }
        let manifest_path = self.base_dir.join("content").join("net-manifest.ron");
        if manifest_path.exists() {
            let mut manifest = ContentManifest::load(&manifest_path)
                .map_err(|e| HostError::new(ExitCode::Content, e.to_string()))?;
            // A packaged file holds no protocol hash: it hashes the types this process registered.
            if manifest.protocol_hash == 0 {
                manifest.protocol_hash = game.session().manifest().protocol_hash;
            }
            game.session().set_manifest(manifest);
        }
        let bans = BanStore::open(&self.base_dir.join("bans.ron"));
        // Only the starting row must exist. A later missing row is skipped at `next_level`.
        let start_row = self
            .playlist
            .entries
            .get(self.rotation_index)
            .cloned()
            .ok_or_else(|| HostError::new(ExitCode::Content, "levels.rotation: no starting level"))?;
        if !game.level_exists(&start_row.map_id) {
            return Err(HostError::new(
                ExitCode::Content,
                format!(
                    "levels.rotation: `{}` is not in the server content",
                    start_row.map_id
                ),
            ));
        }
        // ADR-8881: the game's rule sets resolve every row; the starting row must resolve, and the
        // playlist's rules hash is in the manifest before the session is hosted (when the game
        // opts in).
        self.rule_sets = game.rule_sets();
        self.rules_in_manifest = self.rule_sets.is_some() && game.rules_in_manifest();
        if let Some(library) = &self.rule_sets {
            let index = u32::try_from(self.rotation_index).unwrap_or(u32::MAX);
            let rules = library.playlist_rules(&self.playlist.entries);
            if let Some((_, error)) = rules.refused.iter().find(|(row, _)| *row == index) {
                return Err(HostError::new(
                    ExitCode::Content,
                    format!(
                        "levels.playlist: the starting row's mode `{}`: {error}",
                        start_row.mode
                    ),
                ));
            }
        }
        self.refresh_rules_hash(game);
        self.level = start_row.map_id.clone();
        let token = self.read_admin_token(transport);
        self.remote_admin = token.is_some();
        // The game's sender takes the named `[net]` rates first, so the session advertises the
        // cadence the server really sends at (ADR-7213).
        let named = self.net_rates();
        game.set_net_rates(&named);
        let in_use = game.net_rates_in_use();
        warn_unreported_net_rates(&named, &in_use);
        let opts = self.host_options(token, &in_use);
        let networked = transport != HostTransport::Local;
        let identity = (networked && self.settings.encryption.can_encrypt())
            .then(|| self.load_identity())
            .transpose()?;
        let addr = match transport {
            HostTransport::UdpAnyPort => SocketAddr::new(self.settings.bind, 0),
            HostTransport::Udp | HostTransport::Local => self.settings.game_addr(),
        };
        // A replication game binds the game port with its own link (ADR-2440).
        let port = GamePort {
            timeout: self.settings.timeout,
            addr,
            crypto: self.settings.encryption,
            identity: identity.clone(),
            max_players: self.settings.max_players,
            password: (!self.settings.password.is_empty()).then(|| self.settings.password.clone()),
            tick_hz: self.settings.tick_hz,
            mtu_probing: self.settings.mtu_probing,
        };
        // `--shared-port`: one socket, the link and the session each on a half (ADR-2665 (6)).
        let shared = if networked && self.settings.shared_port {
            game.game_port_protocol()
        } else {
            None
        };
        if networked && self.settings.shared_port && shared.is_none() {
            tracing::warn!(
                "server host: --shared-port needs a replication game (HostedLink); the session binds the port"
            );
        }
        let mut session_half = None;
        // `--relay`: the relay is reached from the game socket (spec 120 § 10.7, ADR-8183), so
        // the mapping it records is the game's; its control traffic is split off before the
        // endpoint.
        let relay_at = if networked { self.prepare_relay()? } else { None };
        let mut relay_control: Option<RelayControlPort<crate::socket::UdpTransport>> = None;
        let bind_game = |relay_control: &mut Option<RelayControlPort<crate::socket::UdpTransport>>| {
            let socket = crate::socket::UdpTransport::bind(addr)
                .map_err(|e| HostError::new(ExitCode::Bind, format!("server.port {}: {e}", addr.port())))?;
            Ok::<crate::link::LinkSocket, HostError>(match relay_at {
                Some(relay) => {
                    let (game_half, control) = relay_split(socket, relay);
                    *relay_control = Some(control);
                    Box::new(game_half)
                }
                None => Box::new(socket),
            })
        };
        let own = match shared {
            Some(protocol) => {
                let socket = bind_game(&mut relay_control)?;
                let (handle, session_side, link_side) = SharedPort::split(socket, protocol);
                session_half = Some(session_side);
                self.shared_port = Some(handle);
                game.host_game_port_on(&port, Box::new(link_side))
            }
            None if relay_at.is_some() && game.game_port_protocol().is_some() => {
                let socket = bind_game(&mut relay_control)?;
                game.host_game_port_relayed(&port, socket)
            }
            None if networked => game.host_game_port(&port),
            None => None,
        };
        let own_addr = match own {
            Some(Ok(bound)) => Some(bound),
            Some(Err((code, message))) => {
                let exit = if code == "link_password_plain" {
                    ExitCode::Config
                } else {
                    ExitCode::Bind
                };
                return Err(HostError::new(
                    exit,
                    format!("server.port {}: {code}: {message}", addr.port()),
                ));
            }
            None => None,
        };
        let session = game.session();
        session.set_settings(SessionSettings {
            timeout_ms: self.settings.timeout.timeout_ms,
            timeout_s: self.settings.timeout.timeout_ms as f64 / 1000.0,
            interrupt_ms: self.settings.timeout.interrupt_ms,
            start_grace_ms: self.settings.timeout.start_grace_ms,
            encryption: self.settings.encryption,
            ..session.settings().clone()
        });
        session.set_identity(identity);
        let hosted = match session_half {
            Some(half) => session.host_on(half, opts, true),
            None if own_addr.is_some() || !networked => session.host_local(opts, true),
            None if relay_at.is_some() => session.host_on(bind_game(&mut relay_control)?, opts, true),
            None => session.host(addr, opts, true),
        };
        hosted.map_err(|e| {
            let exit = if e == SessionError::PasswordNeedsEncryption {
                ExitCode::Config
            } else {
                ExitCode::Bind
            };
            HostError::new(exit, format!("server.port {}: {e}", self.settings.port))
        })?;
        if let Some(server) = session.server_mut() {
            server.set_bans(bans);
        }
        self.game_owns_port = own_addr.is_some();
        self.game_port = own_addr.or_else(|| if networked { session.local_addr() } else { None });
        let key = if self.game_owns_port {
            game.game_port_key()
        } else {
            game.session().server_key()
        };
        if let Some(bound) = self.game_port {
            let key_text = key.map_or_else(|| "none".to_owned(), |k| k.to_string());
            tracing::info!(addr = %bound, server_key = %key_text, game_owns_port = self.game_owns_port, "server host: listening");
            self.console_out.push(format!("listening {bound} key {key_text}"));
        }
        if let Some(relay) = relay_at
            && relay_control.is_some()
        {
            // Every relayed client shares the relay's IP (spec 120 § 10.6).
            game.session().add_relay_ip(relay.ip());
            game.add_game_port_relay_ip(relay.ip());
        }
        self.start_relay(key, relay_at, relay_control)?;
        if self.settings.status_enabled {
            let health = HealthServer::start(self.settings.status_addr()).map_err(|e| {
                HostError::new(
                    ExitCode::Bind,
                    format!("status.port {}: {e}", self.settings.status_port),
                )
            })?;
            self.health = Some(health);
        }
        self.state = HostState::Loading;
        tracing::info!(phase = "level_load", level = %self.level, "server host: loading");
        self.emit_rotation_row(game, &start_row);
        game.load_level(&self.level)
            .map_err(|e| HostError::new(ExitCode::Content, format!("level `{}`: {e}", self.level)))?;
        // A kept world comes back before any session update lets a client in (ADR-3354).
        game.host_event(&HostEvent::Started {
            level: &self.level,
            base_dir: &self.base_dir,
            settings: &self.settings,
        })
        .map_err(|e| HostError::new(ExitCode::Content, format!("level `{}`: {e}", self.level)))?;
        self.state = HostState::Running;
        let now = self.clock.now();
        self.next_tick = now;
        self.started = now;
        self.last_minute_log = now;
        tracing::info!(phase = "running", "server host: running");
        game.install_link(self.link.clone());
        self.publish_health(game, true);
        Ok(())
    }

    /// `--relay`: parses it and starts the local relay when asked for; the relay's address.
    fn prepare_relay(&mut self) -> Result<Option<SocketAddr>, HostError> {
        let Some(text) = self.cli.relay.clone() else {
            return Ok(None);
        };
        let spec =
            RelaySpec::parse(&text).map_err(|e| HostError::new(ExitCode::Config, format!("--relay: {e}")))?;
        Ok(Some(match spec {
            RelaySpec::Local(port) => {
                let secret = crate::handshake::fresh_secret();
                let mut seed = [0u8; 8];
                seed.copy_from_slice(&secret[..8]);
                let local = LocalRelay::bind(port, u64::from_le_bytes(seed))
                    .map_err(|e| HostError::new(ExitCode::Bind, format!("--relay: {e}")))?;
                let at = local.addr();
                tracing::info!(relay = %at, "server host: local relay running");
                self.console_out.push(format!("relay {at}"));
                self.local_relay = Some(local);
                at
            }
            RelaySpec::Remote(at) => at,
        }))
    }

    /// `--relay`: the registrar of the game port's ticket. With the control half of the game
    /// socket it registers from the game socket (spec 120 § 10.7); a game that binds its own port
    /// without a link protocol keeps the separate registrar socket and the port in the ticket.
    fn start_relay(
        &mut self,
        key: Option<ServerKey>,
        relay: Option<SocketAddr>,
        control: Option<RelayControlPort<crate::socket::UdpTransport>>,
    ) -> Result<(), HostError> {
        let Some(relay) = relay else {
            return Ok(());
        };
        let Some(addr) = self.game_port else {
            tracing::warn!("server host: --relay ignored: no network game port (local transport)");
            return Ok(());
        };
        let on_game_socket = control.is_some();
        let registrar = match control {
            Some(control) => RelayRegistrar::new(Box::new(control), relay, &JoinTicket::from_game_socket(key)),
            None => {
                tracing::warn!(
                    "server host: the game binds its own port: the relay registration uses its own socket, so a host behind a NAT is reachable only directly"
                );
                RelayRegistrar::udp(
                    relay,
                    &JoinTicket {
                        addr,
                        server_key: key,
                    },
                )
            }
        }
        .map_err(|e| HostError::new(ExitCode::Bind, format!("--relay: {e}")))?;
        let mut registrar = registrar;
        let hide = self.cli.relay_only;
        if hide {
            registrar = registrar.relay_only();
        }
        if on_game_socket && !hide {
            // The hosting flow (spec 120 § 10.7): map the game port, check that a stranger reaches
            // it, and measure the uplink. A relay on this machine has no NAT between it and the
            // host: nothing to map or measure.
            let now_ms = self.clock.now().as_millis() as u64;
            let remote = !relay.ip().is_loopback();
            let mapping = port_map_choice(relay, &self.cli);
            registrar.start_host_check(
                crate::host_check::HostCheckConfig {
                    measure_uplink: remote,
                    ..crate::host_check::HostCheckConfig::default()
                },
                now_ms,
            );
            if let Err(code) = mapping {
                registrar.clear_port_mapping(code, now_ms);
            } else {
                let client_ip = crate::nat::host_candidate_addrs(addr, relay)
                    .into_iter()
                    .find_map(|a| match a.ip() {
                        std::net::IpAddr::V4(ip) => Some(ip),
                        std::net::IpAddr::V6(_) => None,
                    });
                self.port_map = Some(match client_ip {
                    Some(ip) => crate::port_map::PortMapWorker::start(addr.port(), ip),
                    None => crate::port_map::PortMapWorker::failed("port_map_unavailable"),
                });
            }
        }
        self.registrar = Some(registrar);
        Ok(())
    }

    /// Runs the local relay and the registrar for one tick, and once a second publishes their
    /// report for `net.relay_status` (ADR-8183).
    fn pump_relay(&mut self, game: &mut dyn HostGame) {
        let now_ms = self.clock.now().as_millis() as u64;
        if let Some(relay) = &mut self.local_relay {
            relay.update(now_ms);
        }
        if let (Some(worker), Some(registrar)) = (&self.port_map, &mut self.registrar) {
            for event in worker.poll() {
                match event {
                    crate::port_map::PortMapEvent::Mapped {
                        mapper,
                        mapping,
                        renewals,
                    } => {
                        if renewals == 0 {
                            tracing::info!(mapper, external = %mapping.external, "server host: port mapped");
                            self.console_out
                                .push(format!("port mapped {} ({mapper})", mapping.external));
                        }
                        registrar.set_port_mapping(mapper, &mapping, renewals, now_ms);
                    }
                    crate::port_map::PortMapEvent::Failed(code) => registrar.clear_port_mapping(code, now_ms),
                    crate::port_map::PortMapEvent::Lapsed => {
                        tracing::warn!("server host: the port mapping lapsed");
                        registrar.clear_port_mapping("port_map_lapsed", now_ms);
                    }
                }
            }
        }
        let learned = self.registrar.as_mut().is_some_and(|r| r.update(now_ms));
        if let Some(relay) = &mut self.local_relay {
            relay.update(now_ms);
        }
        if !self.host_check_logged
            && let Some(check) = self.registrar.as_ref().and_then(RelayRegistrar::host_check)
            && check.done
        {
            self.host_check_logged = true;
            let clients = check
                .estimated_clients
                .map_or_else(|| "unknown".to_owned(), |n| n.to_string());
            tracing::info!(
                reachable = %check.reachable,
                mapping = ?check.mapping.as_ref().map(|m| &m.external),
                uplink_bytes_per_second = ?check.uplink_bytes_per_second,
                estimated_clients = %clients,
                "server host: host check"
            );
            self.console_out.push(format!(
                "host check reachable {} clients {clients}",
                check.reachable
            ));
        }
        if learned && let Some(registrar) = &self.registrar {
            let code = registrar.join_code().unwrap_or_default().to_owned();
            tracing::info!(join_code = %code, relay = %registrar.relay(), "server host: join code");
            self.console_out
                .push(format!("join code {code} relay {}", registrar.relay()));
        }
        if (self.registrar.is_some() || self.local_relay.is_some()) && now_ms >= self.next_relay_report_ms {
            self.next_relay_report_ms = now_ms.saturating_add(1000);
            game.publish_relay(&self.relay_report());
        }
    }

    /// The host's relay view (`net.relay_status`): its registration, and the local relay's
    /// counters, caps and allocations with `--relay local`.
    pub fn relay_report(&self) -> crate::relay::RelayReport {
        let mut report = self
            .registrar
            .as_ref()
            .map(RelayRegistrar::report)
            .unwrap_or_default();
        if let Some(local) = &self.local_relay {
            let relay = local.report();
            report.role = if self.registrar.is_some() {
                "host+relay"
            } else {
                "relay"
            }
            .to_owned();
            report.relay = relay.relay;
            report.status = relay.status;
            report.config = relay.config;
            report.allocations = relay.allocations;
        }
        report
    }

    /// Players on the game port: the link's peers when the game owns the port (with the session's
    /// on a shared port, ADR-2665 (6)), else the session's.
    fn players(&self, game: &mut dyn HostGame) -> u32 {
        if self.game_owns_port {
            let link = game
                .game_port_peers()
                .map_or(0, |p| u32::try_from(p.len()).unwrap_or(u32::MAX));
            let session = if self.shared_port.is_some() {
                game.session().server().map_or(0, |s| s.players_in_use())
            } else {
                0
            };
            return link.saturating_add(session);
        }
        game.session().server().map_or(0, |s| s.players_in_use())
    }

    fn period(&self, game: &mut dyn HostGame) -> Duration {
        let idle = self.state == HostState::Running
            && if self.game_owns_port {
                self.players(game) == 0
                    && (self.shared_port.is_none()
                        || game.session().server().is_none_or(|s| s.handshaking() == 0))
            } else {
                game.session()
                    .server()
                    .is_some_and(|s| s.players_in_use() == 0 && s.handshaking() == 0)
            };
        let hz = if idle && game.allow_idle_slowdown() {
            self.settings.idle_tick_hz
        } else {
            self.settings.tick_hz
        };
        game.set_tick_hz(hz);
        Duration::from_secs_f64(1.0 / f64::from(hz.max(1)))
    }

    /// One pacing iteration: waits for the next tick (through the clock), then runs the due fixed
    /// steps (one, or up to [`HostGame::max_substeps`] after an overrun). Returns the exit code
    /// once stopped.
    pub fn run_once(&mut self, game: &mut dyn HostGame) -> Option<ExitCode> {
        if let Some(code) = self.exit {
            return Some(code);
        }
        let period = self.period(game);
        if self.clock.now() < self.next_tick {
            let at = self.next_tick;
            self.clock.sleep_until(at);
        }
        let behind = self.clock.now().saturating_sub(self.next_tick);
        let cap = game.max_substeps().clamp(1, regia_app::MAX_SUBSTEPS_LIMIT);
        let due = (1 + (behind.as_secs_f64() / period.as_secs_f64()) as u32).min(cap);
        for _ in 0..due {
            let t0 = self.clock.now();
            self.tick_once(game);
            let dt = self.clock.now().saturating_sub(t0);
            self.stats
                .record(self.clock.now(), dt.as_secs_f64() * 1000.0, dt > period);
            self.net_stats.record_loop(dt.as_secs_f64() * 1000.0);
            if let Some(code) = self.exit {
                return Some(code);
            }
        }
        self.next_tick += period * due;
        if self.clock.now() > self.next_tick + period * cap {
            // Too far behind: drop the backlog instead of spiralling.
            self.next_tick = self.clock.now();
        }
        None
    }

    /// Runs until stopped.
    pub fn run(&mut self, game: &mut dyn HostGame) -> ExitCode {
        loop {
            if let Some(code) = self.run_once(game) {
                return code;
            }
        }
    }

    /// Enters `Draining` with a flush grace (seconds).
    pub fn begin_drain(&mut self, game: &mut dyn HostGame, grace_s: f64) {
        if self.state == HostState::Draining || self.state == HostState::Stopped {
            return;
        }
        self.state = HostState::Draining;
        self.signals.mark_draining();
        self.drain_until = Some(self.clock.now() + Duration::from_secs_f64(grace_s.clamp(0.0, 600.0)));
        self.drain_ticks = 0;
        tracing::info!(phase = "draining", "server host: draining");
        game.session().disconnect();
        if self.game_owns_port {
            game.close_game_port();
        }
        self.publish_health(game, true);
    }

    fn stop(&mut self, game: &mut dyn HostGame) {
        if let Err(e) = game.host_event(&HostEvent::Stopping { tick: self.tick }) {
            tracing::error!(error = %e, "server host: a stop hook failed");
        }
        // Delete the port mapping at the gateway (spec 120 § 10.7).
        if let Some(mut worker) = self.port_map.take() {
            worker.stop();
        }
        self.state = HostState::Stopped;
        tracing::info!(
            phase = "stopped",
            ticks = self.tick,
            overruns = self.stats.overruns_total,
            "server host: stopped"
        );
        self.publish_health(game, true);
        game.host_stopped();
        self.exit = Some(ExitCode::Ok);
    }

    fn tick_once(&mut self, game: &mut dyn HostGame) {
        let _span = tracing::trace_span!("server_host_tick").entered();
        if self.signals.take_shutdown() {
            self.shutdown_requests += 1;
            if self.shutdown_requests >= 2 || self.state == HostState::Draining {
                tracing::info!("server host: second shutdown request; exiting now");
                self.stop(game);
                return;
            }
            let grace = lock(&self.link.shared)
                .drain_grace_s
                .take()
                .unwrap_or(DEFAULT_DRAIN_GRACE_S);
            self.begin_drain(game, grace);
        }
        if self.signals.take_reload() {
            let report = self.reload(game);
            for line in reload_lines(&report) {
                tracing::info!("{line}");
            }
        }
        if self.state == HostState::Running
            && self.signals.take_rotation_skip()
            && let Err(error) = self.next_level(game)
        {
            tracing::warn!(error = %error, "server host: rotation skip left the current level");
        }
        while let Ok(line) = self.console_rx.try_recv() {
            let out = self.execute(game, &line);
            for l in &out {
                tracing::info!(target: "server_console", "{l}");
            }
            self.console_out.extend(out);
        }
        let step_start = self.clock.now();
        game.step();
        let step = self.clock.now().saturating_sub(step_start);
        self.net_stats.record_tick(step.as_secs_f64() * 1000.0);
        if self.game_owns_port {
            // Bans of game-port peers, including entries edited into bans.ron (ADR-2665 (4)).
            game.enforce_game_port_bans();
        }
        self.tick += 1;
        if let Err(e) = game.host_event(&HostEvent::Ticked { tick: self.tick }) {
            tracing::warn!(error = %e, "server host: a tick hook failed");
        }
        self.pump_relay(game);
        // A travel started from inside the game (agent `server.change_level`): follow it. A level
        // whose load fails keeps `self.level` (status, `/health` and the rotation bookkeeping
        // report the level the game actually runs) and is not retried every tick; the next
        // travel (a new travel id, even to the same level) tries again.
        let session_level = game
            .session()
            .server()
            .map(|s| (s.level().to_owned(), s.travel_id()));
        if let Some((level, travel_id)) = session_level
            && level != self.level
            && self.travel_failed != Some(travel_id)
        {
            if let Err(e) = game.host_event(&HostEvent::LevelLeaving {
                from: &self.level,
                to: &level,
            }) {
                tracing::warn!(error = %e, "server host: a level-leave hook failed");
            }
            match game.load_level(&level) {
                Ok(()) => {
                    self.travel_failed = None;
                    self.level = level.clone();
                    if let Err(e) = game.host_event(&HostEvent::LevelLoaded { level: &level }) {
                        tracing::error!(%level, error = %e, "server host: a level-loaded hook failed");
                    }
                }
                Err(e) => {
                    self.travel_failed = Some(travel_id);
                    tracing::error!(%level, error = %e, "server host: level load after an in-game travel failed");
                }
            }
        }
        let admin = game
            .session()
            .server_mut()
            .map(|s| s.take_admin_commands())
            .unwrap_or_default();
        for (peer, line) in admin {
            tracing::info!(%peer, %line, "server host: remote admin command");
            let out = self.execute(game, &line);
            if let Some(server) = game.session().server_mut() {
                server.admin_reply(peer, out);
            }
        }
        let now = self.clock.now();
        match self.state {
            HostState::Running => {
                if self.match_end_at.is_none() && game.match_over() {
                    self.match_end_at = Some(now + Duration::from_secs_f64(self.settings.match_end_delay_s));
                }
                if self.match_end_at.is_some_and(|t| now >= t) {
                    self.match_end_at = None;
                    if let Err(error) = self.next_level(game) {
                        tracing::warn!(
                            error = %error,
                            "server host: match end left the current level"
                        );
                    }
                }
            }
            HostState::Draining => {
                self.drain_ticks += 1;
                // A game port the game owns was closed with a goodbye to every peer: nothing to wait
                // for, unless the session shares that port and has peers of its own to see off.
                let empty = (self.game_owns_port && self.shared_port.is_none())
                    || game.session().server().is_none_or(|s| s.peer_ids().is_empty());
                if (empty && self.drain_ticks > 3) || self.drain_until.is_some_and(|t| now >= t) {
                    self.stop(game);
                    return;
                }
            }
            _ => {}
        }
        if now.saturating_sub(self.last_minute_log) >= Duration::from_secs(60) {
            self.last_minute_log = now;
            let players = self.players(game);
            tracing::info!(
                players,
                tick_ms_p50 = self.stats.percentile_ms(50.0),
                tick_ms_p99 = self.stats.percentile_ms(99.0),
                overruns = self.stats.overruns_last_min(),
                "server host: minute stats"
            );
        }
        let now_ms = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
        if self.net_stats.due(now_ms) {
            let samples = game.session().peer_stats_samples();
            self.net_stats.poll(now_ms, &samples);
        }
        self.publish_health(game, false);
    }

    /// The operator stats reporter (spec 120 § 13.2, ADR-8182).
    pub fn net_stats_mut(&mut self) -> &mut crate::stats::StatsReporter {
        &mut self.net_stats
    }

    /// Travels to `level` now (console `level`, agent `server.change_level`).
    pub fn change_level(&mut self, game: &mut dyn HostGame, level: &str) -> Result<u32, String> {
        if !game.level_exists(level) {
            return Err("level_not_in_pak".into());
        }
        if let Err(e) = game.host_event(&HostEvent::LevelLeaving {
            from: &self.level,
            to: level,
        }) {
            tracing::warn!(error = %e, "server host: a level-leave hook failed");
        }
        game.load_level(level)?;
        if let Err(e) = game.host_event(&HostEvent::LevelLoaded { level }) {
            tracing::error!(%level, error = %e, "server host: a level-loaded hook failed");
        }
        let travel = game.session().travel(level).map_err(|e| e.code().to_owned())?;
        self.level = level.to_owned();
        self.travel_failed = None;
        tracing::info!(%level, travel, "server host: travel");
        Ok(travel)
    }

    /// Travels to the next playlist row.
    ///
    /// Takes the row the playlist's own walk takes ([`regia_rules::playlist::Playlist::take_next_for`],
    /// ADR-8897) with the players on the game port and the game's installed levels: a row
    /// outside its player gate, or whose level is missing, is passed over without being recorded
    /// as played, so it is not held back by the repeat guard later. A missing level is logged as
    /// `server_next_level_failed` and emitted as [`HostEvent::NextLevelFailed`]. A row whose level
    /// fails to load is reported the same way, listed as `load_failed`, and not tried again in
    /// this call; the walk goes on to the next row. Every passed row is listed in
    /// [`RotationView::skipped`]. A weighted cycle that lists a row more than once is walked to
    /// its end (a repeated row index is not a wrap). The walk never retries in a loop: each row
    /// is loaded at most once per call. When no row fits, the playlist state and the current
    /// level stay, and the error is the last reason.
    pub fn next_level(&mut self, game: &mut dyn HostGame) -> Result<u32, String> {
        let saved = self.playlist.clone();
        let len = self.playlist.entries.len();
        let players = self.players(game);
        let mut last_err = String::from("rotation_empty");
        self.rotation_skips.clear();
        if len == 0 {
            self.note_failure(game, "", "", &last_err);
            self.publish_rotation();
            return Err(last_err);
        }
        // Levels that failed to load in this call: refused as content from then on, so each row
        // is loaded at most once and the walk ends after at most one load per row.
        let mut failed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut noted: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for _ in 0..=len {
            // `take_next_for` notes the row it hands out as played; a row that then fails to load
            // was never played, so its note is undone below (rules-review-fixes-b-verification
            // finding 2).
            let recent = self.playlist.recent.clone();
            let rule_sets = self.rule_sets.as_ref();
            let pick = self.playlist.take_next_for(players, |row| {
                !failed.contains(&row.map_id)
                    && rule_error(rule_sets, row).is_none()
                    && game.level_exists(&row.map_id)
            });
            for skip in &pick.skipped {
                let Some(row) = self.playlist.entries.get(skip.index as usize).cloned() else {
                    continue;
                };
                // A row that failed to load this call is already listed as `load_failed`.
                if failed.contains(&row.map_id) || !noted.insert(skip.index) {
                    continue;
                }
                let rules = (skip.reason == regia_rules::playlist::SkipReason::MissingContent)
                    .then(|| rule_error(self.rule_sets.as_ref(), &row))
                    .flatten();
                if let Some(error) = rules {
                    // ADR-8881: the row's rule set does not resolve; its code is the reason.
                    last_err = error.code.clone();
                    self.note_failure(game, &row.map_id, &row.mode, &error.to_string());
                    self.note_skip(skip.index, &row, &error.code);
                    continue;
                }
                if skip.reason == regia_rules::playlist::SkipReason::MissingContent {
                    last_err = "level_not_in_pak".into();
                    self.note_failure(game, &row.map_id, &row.mode, &last_err);
                } else {
                    last_err = skip.reason.code().into();
                }
                self.note_skip(skip.index, &row, skip.reason.code());
            }
            let Some((idx, row)) = pick.row else {
                break;
            };
            self.emit_rotation_row(game, &row);
            match self.change_level(game, &row.map_id) {
                Ok(travel) => {
                    self.rotation_index = idx as usize;
                    self.match_end_at = None;
                    self.publish_rotation();
                    return Ok(travel);
                }
                Err(error) => {
                    last_err = error;
                    // Put the repeat guard back as it was before this take: the row never loaded,
                    // and an older row the note pushed out of a full guard comes back.
                    self.playlist.recent = recent;
                    self.note_failure(game, &row.map_id, &row.mode, &last_err);
                    self.note_skip(idx, &row, "load_failed");
                    noted.insert(idx);
                    failed.insert(row.map_id.clone());
                }
            }
        }
        self.playlist = saved;
        self.publish_rotation();
        Err(last_err)
    }

    /// Recomputes the playlist's rules hash from the rule sets read at start and, when the game
    /// opted in, sets it on the session (ADR-8881). A running session keeps the manifest it
    /// started with; the hash is for the next session start and for `server.rotation_get`.
    fn refresh_rules_hash(&mut self, game: &mut dyn HostGame) {
        let Some(library) = &self.rule_sets else {
            return;
        };
        let rules = library.playlist_rules(&self.playlist.entries);
        for (index, error) in &rules.refused {
            tracing::warn!(index, error = %error, "server rotation: a playlist row's rule set does not resolve");
        }
        self.rules_hash = rules.hash;
        if self.rules_in_manifest {
            game.session().set_rules_hash(rules.hash);
        }
    }

    fn note_skip(&mut self, index: u32, row: &regia_rules::playlist::PlaylistEntry, reason: &str) {
        tracing::info!(map_id = %row.map_id, mode = %row.mode, reason, "server rotation: row skipped");
        self.rotation_skips.push(RotationSkip {
            index,
            map_id: row.map_id.clone(),
            mode: row.mode.clone(),
            reason: reason.to_owned(),
        });
    }

    fn emit_rotation_row(&mut self, game: &mut dyn HostGame, row: &regia_rules::playlist::PlaylistEntry) {
        let pairs: Vec<(&str, &str)> = row
            .options
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if let Err(error) = game.host_event(&HostEvent::RotationRow {
            map_id: &row.map_id,
            mode: &row.mode,
            options: &pairs,
        }) {
            tracing::warn!(error = %error, "server host: a rotation-row hook failed");
        }
    }

    fn note_failure(&mut self, game: &mut dyn HostGame, map_id: &str, mode: &str, error: &str) {
        tracing::warn!(map_id, mode, error, "server_next_level_failed");
        if let Err(hook) = game.host_event(&HostEvent::NextLevelFailed { map_id, mode, error }) {
            tracing::warn!(error = %hook, "server host: a next-level-failed hook failed");
        }
        self.last_failure = Some(RotationFailure {
            map_id: map_id.to_owned(),
            mode: mode.to_owned(),
            error: error.to_owned(),
        });
    }

    fn publish_rotation(&self) {
        lock(&self.link.shared).rotation = rotation_view_of(
            &self.playlist,
            self.rotation_index,
            &self.last_failure,
            &self.rotation_skips,
            self.rules_hash,
            self.rules_in_manifest,
        );
    }

    /// Re-reads `server.toml` (+ environment + command line) and `bans.ron`; applies the hot keys.
    pub fn reload(&mut self, game: &mut dyn HostGame) -> ReloadReport {
        let mut report = ReloadReport::default();
        let resolved = match resolve_config(&self.base_dir, &self.cli, &*self.env) {
            Ok(r) => r,
            Err(errors) => {
                for e in errors {
                    tracing::warn!("server host: reload refused: {e}");
                }
                return report;
            }
        };
        let new = resolved.settings;
        report.applied = self
            .settings
            .hot_changes(&new)
            .into_iter()
            .map(str::to_owned)
            .collect();
        report.needs_restart = self
            .settings
            .restart_only_changes(&new)
            .into_iter()
            .map(str::to_owned)
            .collect();
        for key in &report.needs_restart {
            tracing::warn!(%key, "server host: reload ignored a change that needs a restart");
        }
        // Apply the hot keys only.
        self.settings.name = new.name;
        self.settings.motd = new.motd;
        self.settings.max_players = new.max_players;
        self.settings.password = new.password;
        if self.settings.rotation != new.rotation || self.settings.playlist != new.playlist {
            self.settings.rotation.clone_from(&new.rotation);
            self.settings.playlist.clone_from(&new.playlist);
            match load_playlist(&self.base_dir, &self.settings) {
                Ok(mut loaded) => {
                    if self.rotation_index >= loaded.entries.len() {
                        self.rotation_index = 0;
                    }
                    loaded.start_at(u32::try_from(self.rotation_index).unwrap_or(u32::MAX));
                    self.playlist = loaded;
                    self.refresh_rules_hash(game);
                    self.publish_rotation();
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "server host: playlist reload failed; keeping the list"
                    );
                }
            }
        }
        self.settings.client_rate_bytes = new.client_rate_bytes;
        self.settings.server_rate_bytes = new.server_rate_bytes;
        self.settings.send_every_ticks = new.send_every_ticks;
        self.settings.entropy = new.entropy;
        let token = game
            .session()
            .server()
            .and_then(|s| s.options().admin_token.clone());
        let named = self.net_rates();
        game.set_net_rates(&named);
        let in_use = game.net_rates_in_use();
        warn_unreported_net_rates(&named, &in_use);
        let opts = self.host_options(token, &in_use);
        if let Some(server) = game.session().server_mut() {
            server.set_options(opts);
            match server.bans_mut().reload() {
                Ok(()) => report.applied.push("bans.ron".into()),
                Err(e) => tracing::warn!(error = %e, "server host: bans.ron reload failed; keeping the list"),
            }
        }
        tracing::info!(applied = ?report.applied, "server host: reloaded");
        report
    }

    fn publish_health(&mut self, game: &mut dyn HostGame, force: bool) {
        let now = self.clock.now();
        if !force
            && self
                .last_health
                .is_some_and(|t| now.saturating_sub(t) < Duration::from_secs(1))
        {
            return;
        }
        self.last_health = Some(now);
        let json = self.health_json(game);
        {
            let mut shared = lock(&self.link.shared);
            shared.health_json.clone_from(&json);
            shared.state = self.state;
            shared.settings.clone_from(&self.settings);
            shared.rotation = rotation_view_of(
                &self.playlist,
                self.rotation_index,
                &self.last_failure,
                &self.rotation_skips,
                self.rules_hash,
                self.rules_in_manifest,
            );
        }
        if let Some(h) = &self.health {
            h.publish(HealthSnapshot {
                json,
                draining: self.state == HostState::Draining,
            });
        }
    }

    /// The `/health` JSON (spec 76 § 8).
    pub fn health_json(&self, game: &mut dyn HostGame) -> String {
        let players = self.players(game);
        // A lockstep server adds its rows (ADR-8558); every other game's body is unchanged.
        let lockstep = game
            .lockstep_status()
            .map_or_else(String::new, |s| format!(",\"lockstep\":{}", s.health_json()));
        format!(
            "{{\"ok\":{},\"state\":{},\"tick\":{},\"players\":{},\"max_players\":{},\"level\":{},\"uptime_s\":{},\"tick_ms_p99\":{:.2},\"overruns_last_min\":{},\"game_port\":{},\"join_code\":{}{lockstep}}}",
            self.state == HostState::Running,
            json_str(self.state.as_str()),
            self.tick,
            players,
            self.settings.max_players,
            json_str(&self.level),
            self.clock.now().saturating_sub(self.started).as_secs(),
            self.stats.percentile_ms(99.0),
            self.stats.overruns_last_min(),
            self.game_port
                .map_or_else(|| "null".to_owned(), |a| json_str(&a.to_string())),
            self.join_code().map_or_else(|| "null".to_owned(), json_str),
        )
    }

    /// Kicks (console `kick`, agent `server.kick`).
    pub fn kick(&mut self, game: &mut dyn HostGame, player: &str, reason: &str) -> Result<(), String> {
        let mut target = player;
        if self.game_owns_port {
            let (layer, id) = game_port_player(game, player)?;
            if layer == PlayerLayer::Link {
                let kicked = parse_player_id(id).is_some_and(|(peer, _)| game.kick_game_peer(peer));
                if !kicked {
                    return Err(op_err("player_unknown", player.to_owned()).to_string());
                }
                tracing::info!(%player, %reason, "server host: kick (game port)");
                return Ok(());
            }
            target = id;
        }
        kick_player(game.session(), target, reason).map_err(|e| e.to_string())
    }

    /// Bans and kicks (console `ban`, agent `server.ban`).
    pub fn ban(
        &mut self,
        game: &mut dyn HostGame,
        player: &str,
        duration_s: Option<u64>,
        reason: &str,
    ) -> Result<BanId, String> {
        let mut target = player;
        if self.game_owns_port {
            let (layer, id) = game_port_player(game, player)?;
            if layer == PlayerLayer::Link
                && let Some(banned) = game.ban_game_peer(id, duration_s, reason)
            {
                return banned.map_err(|e| e.to_string());
            }
            target = id;
        }
        ban_player(game.session(), target, duration_s, reason).map_err(|e| e.to_string())
    }

    /// Removes a ban entry.
    pub fn unban(&mut self, game: &mut dyn HostGame, id: u64) -> Result<(), String> {
        if self.game_owns_port
            && let Some(r) = game.unban_game_entry(id)
        {
            return r.map_err(|e| e.to_string());
        }
        unban_entry(game.session(), id).map_err(|e| e.to_string())
    }

    /// Runs one console line now; returns its output.
    pub fn execute(&mut self, game: &mut dyn HostGame, line: &str) -> Vec<String> {
        let line = line.trim();
        let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let err = |e: String| vec![format!("error: {e}")];
        match cmd {
            "" => Vec::new(),
            "help" => {
                let mut out: Vec<String> = CONSOLE_HELP.iter().map(|s| (*s).to_owned()).collect();
                out.extend(game.console_help());
                out
            }
            "status" => {
                let players = self.players(game);
                let mut out = vec![format!(
                    "name={} level={} tick={} players={}/{} overruns_last_min={} state={}",
                    self.settings.name,
                    self.level,
                    self.tick,
                    players,
                    self.settings.max_players,
                    self.stats.overruns_last_min(),
                    self.state.as_str()
                )];
                // One line per session player with its ping (spec 76 § 6, ADR-5650).
                let links = game.session().peer_link_stats();
                if let Some(server) = game.session().server() {
                    for row in server.peer_rows() {
                        let (ping, _) = link_columns(links.get(&PeerId(row.peer)));
                        out.extend(row.players.iter().map(|id| format!("  {id} ping={ping}")));
                    }
                }
                out
            }
            "players" if self.game_owns_port => {
                let peers = game.game_port_peers().unwrap_or_default();
                let mut out: Vec<String> = peers.iter().map(|p| format!("{} game-port", p.0)).collect();
                // On a shared port the session's players are here too, named so that `kick` and
                // `ban` can take the line's first word as it stands.
                if self.shared_port.is_some()
                    && let Some(server) = game.session().server()
                {
                    for row in server.peer_rows() {
                        for (id, name) in row.players.iter().zip(row.names.iter()) {
                            out.push(format!("session:{id} {name} {} {}", row.addr, row.state));
                        }
                    }
                }
                if out.is_empty() {
                    out.push("no players".into());
                }
                out
            }
            "players" => {
                let links = game.session().peer_link_stats();
                let Some(server) = game.session().server() else {
                    return err("not a server".into());
                };
                let mut out = Vec::new();
                for row in server.peer_rows() {
                    let (ping, loss) = link_columns(links.get(&PeerId(row.peer)));
                    for (id, name) in row.players.iter().zip(row.names.iter()) {
                        out.push(format!(
                            "{id} {name} {} ping={ping} loss={loss} {}",
                            row.addr, row.state
                        ));
                    }
                }
                if out.is_empty() {
                    out.push("no players".into());
                }
                out
            }
            "kick" => {
                let (who, reason) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                let reason = if reason.trim().is_empty() {
                    "kicked by the server"
                } else {
                    reason.trim()
                };
                match self.kick(game, who, reason) {
                    Ok(()) => vec![format!("kicked {who}")],
                    Err(e) => err(e),
                }
            }
            "ban" => {
                let mut parts = rest.splitn(3, char::is_whitespace);
                let who = parts.next().unwrap_or("");
                let second = parts.next().unwrap_or("");
                let (duration, reason) = match parse_duration_s(second) {
                    Some(d) => (Some(d), parts.next().unwrap_or("").to_owned()),
                    None => (
                        None,
                        [second, parts.next().unwrap_or("")].join(" ").trim().to_owned(),
                    ),
                };
                let reason = if reason.is_empty() {
                    "banned by the server".to_owned()
                } else {
                    reason
                };
                match self.ban(game, who, duration, &reason) {
                    Ok(id) => vec![format!("banned {who} (entry {})", id.0)],
                    Err(e) => err(e),
                }
            }
            "unban" => match rest.parse::<u64>() {
                Ok(id) => match self.unban(game, id) {
                    Ok(()) => vec![format!("removed ban entry {id}")],
                    Err(e) => err(e),
                },
                Err(_) => err("usage: unban <entry-id>".into()),
            },
            "bans" => {
                let session = game.session();
                // Active on the session clock, like the join check (ADR-5973).
                let now = session.now_unix_s();
                let Some(server) = session.server() else {
                    return err("not a server".into());
                };
                let mut out: Vec<String> = server
                    .bans()
                    .list()
                    .entries
                    .iter()
                    .filter(|e| e.active(now))
                    .map(|e| {
                        format!(
                            "{} {} {} until={} reason={}",
                            e.id.0,
                            e.kind.as_str(),
                            e.value,
                            e.until.map_or("never".into(), |u| u.to_string()),
                            e.reason
                        )
                    })
                    .collect();
                if out.is_empty() {
                    out.push("no bans".into());
                }
                out
            }
            "level" => match self.change_level(game, rest) {
                Ok(t) => vec![format!("travel {t} to {rest}")],
                Err(e) => err(e),
            },
            "next" => match self.next_level(game) {
                Ok(t) => vec![format!("travel {t} to {}", self.level)],
                Err(e) => err(e),
            },
            "say" => {
                if game.say(rest) {
                    vec![format!("said: {rest}")]
                } else {
                    vec!["say: ignored (the game registers no chat handler)".into()]
                }
            }
            "reload" => reload_lines(&self.reload(game)),
            "joincode" => vec![match self.join_code() {
                Some(code) => format!(
                    "join code {code} relay {}",
                    self.relay_addr().map_or_else(String::new, |a| a.to_string())
                ),
                None => "no join code (start the server with --relay)".to_owned(),
            }],
            "revokecode" => vec![if self.revoke_join_code() {
                "join code withdrawn".to_owned()
            } else {
                "no join code (start the server with --relay)".to_owned()
            }],
            "relay" => vec![match self.relay_status() {
                Some(s) => format!(
                    "relay protocol={} lobbies={} flows={} allocations={} ports_free={}/{} hosts={} most_clients={}/{} forwarded={} forward_dropped={} host_rate_dropped={} permission_dropped={} full_refused={} version_refused={}",
                    s.protocol,
                    s.lobbies,
                    s.flows,
                    s.allocations,
                    s.ports_free,
                    s.ports,
                    s.hosts,
                    s.most_clients_of_one_host,
                    s.max_clients_per_host,
                    s.forwarded,
                    s.forward_dropped,
                    s.host_rate_dropped,
                    s.permission_dropped,
                    s.full_refused,
                    s.version_refused,
                ),
                None => "no local relay (start the server with --relay local)".to_owned(),
            }],
            "serverkey" if self.game_owns_port => vec![match game.game_port_key() {
                Some(key) => format!("server key: {key}"),
                None => "server key: none (this server does not encrypt)".to_owned(),
            }],
            "serverkey" => vec![match game.session().server_key() {
                Some(key) => format!("server key: {key}"),
                None => "server key: none (this server does not encrypt)".to_owned(),
            }],
            "quit" => {
                let grace = if rest.is_empty() {
                    Some(DEFAULT_DRAIN_GRACE_S)
                } else {
                    rest.parse::<f64>().ok()
                };
                match grace {
                    Some(g) if g >= 0.0 => {
                        self.begin_drain(game, g);
                        vec![format!("draining ({g} s)")]
                    }
                    _ => err("usage: quit [grace-seconds]".into()),
                }
            }
            other => game
                .console_command(other, rest)
                .unwrap_or_else(|| vec![format!("unknown command `{other}`; type `help`")]),
        }
    }
}

/// The `ping=` and `loss=` columns of the console's `players` (spec 76 § 6, ADR-5650): whole
/// milliseconds and the server-to-player loss in percent, `-` while the transport has no estimate.
fn link_columns(stats: Option<&crate::session::players::PlayerLinkStats>) -> (String, String) {
    match stats {
        Some(s) => (
            format!("{:.0}ms", s.ping_ms),
            format!("{:.1}%", f64::from(s.loss_to_player) * 100.0),
        ),
        None => ("-".to_owned(), "-".to_owned()),
    }
}

fn reload_lines(r: &ReloadReport) -> Vec<String> {
    let mut out = vec![format!(
        "reload applied: {}",
        if r.applied.is_empty() {
            "nothing".into()
        } else {
            r.applied.join(", ")
        }
    )];
    if !r.needs_restart.is_empty() {
        out.push(format!(
            "needs a restart (ignored): {}",
            r.needs_restart.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_app::prelude::*;

    use super::*;

    #[test]
    fn durations_and_player_ids() {
        assert_eq!(parse_duration_s("30m"), Some(1800));
        assert_eq!(parse_duration_s("12h"), Some(43_200));
        assert_eq!(parse_duration_s("7d"), Some(604_800));
        assert_eq!(parse_duration_s("cheat"), None);
        assert_eq!(parse_duration_s(""), None);
        // Review regressions: a multibyte last character, an overflowing number.
        assert_eq!(parse_duration_s("tricheé"), None);
        assert_eq!(parse_duration_s("18446744073709551615d"), None);
        assert_eq!(parse_player_id("3.0"), Some((PeerId(3), Some(0))));
        assert_eq!(parse_player_id("3"), Some((PeerId(3), None)));
        assert_eq!(parse_player_id("x.1"), None);
    }

    #[test]
    fn json_escapes() {
        assert_eq!(json_str("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }

    /// Audit net1 (platform): when `bans.ron` cannot be saved (a locked file on Windows, a
    /// read-only store), the ban entry stays memory-only but the abusive peer must still be
    /// kicked, and the operator gets the save error under `ban_store_save_failed` (not
    /// `ban_invalid`, and not silence with the peer left in the game).
    #[test]
    fn a_failed_ban_save_still_kicks_the_peer() {
        use crate::session::bans::BanSubject;
        use crate::session::{
            BanStore, ClientSessionEvent, ClientSessionState, ConnectOptions, HostOptions, NetSession,
        };
        use crate::socket::LoopbackNet;
        let net = LoopbackNet::new();
        let saddr: std::net::SocketAddr = "127.0.0.1:7781".parse().expect("addr");
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server
            .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), false)
            .expect("host");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        for _ in 0..200 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(client.state(), ClientSessionState::InGame);
        let peer = client
            .take_client_events()
            .iter()
            .find_map(|e| match e {
                ClientSessionEvent::Welcomed(w) => Some(w.peer_id),
                _ => None,
            })
            .expect("the welcome named the peer");

        // The bans path becomes a directory: the save's temp write works, the final rename fails
        // on every platform (the Windows twin is an editor holding `bans.ron` open without
        // FILE_SHARE_DELETE).
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-host-ban-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let bans_path = dir.join("bans.ron");
        let store = BanStore::open(&bans_path);
        assert!(store.writable(), "a missing file opens writable");
        std::fs::create_dir(&bans_path).expect("a directory at the target breaks the rename");
        server.server_mut().expect("the hosted session").set_bans(store);
        let ip = server
            .server()
            .and_then(|s| s.peer_addr(peer))
            .expect("the peer address")
            .ip();

        let err = ban_player(&mut server, &peer.0.to_string(), None, "abuse")
            .expect_err("the save failure is reported");
        assert_eq!(err.code, "ban_store_save_failed", "not a mislabelled ban_invalid");
        assert!(
            server.server().is_some_and(|s| s.peer_state(peer).is_none()),
            "the banned peer was kicked even though the save failed"
        );
        let now = server.now_unix_s();
        assert!(
            server.server().is_some_and(|s| s
                .bans()
                .list()
                .find(
                    &BanSubject {
                        platform_id: None,
                        addr: Some(ip),
                        names: &[]
                    },
                    now,
                )
                .is_some()),
            "the entry is in memory for this run (it applies until the next restart)"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// corr-net (ADR-5580): when `bans.ron` cannot be written the entry is still active for this
    /// run, so `ban_player` must still remove the player. It used to return `ban_invalid` before
    /// the kick: the banned player stayed in the game while their next join was refused.
    #[test]
    fn corr_net_a_ban_that_is_not_saved_still_kicks_the_player() {
        use crate::session::hub::SimInput;
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-ban-kick-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("bans.ron");
        std::fs::write(&path, "BanList(format_version: 2, entries: [])\n").expect("bans");
        let mut s = NetSession::default();
        s.set_manual_clock(0);
        s.host_local(
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
        s.add_simulated_clients(2, SimInput::Idle).expect("sims");
        for _ in 0..60 {
            s.advance_manual_clock(16);
            s.update();
        }
        let store = BanStore::open(&path);
        assert!(store.writable());
        s.server_mut().expect("server").set_bans(store);
        // The target becomes a directory: the temp file is written, the rename fails.
        std::fs::remove_file(&path).expect("remove");
        std::fs::create_dir(&path).expect("dir at the target");
        assert!(s.peer_state(PeerId(1)).is_some(), "peer 1 joined");
        let err = ban_player(&mut s, "1", None, "spam").expect_err("the save failed");
        assert_eq!(err.code, "ban_store_save_failed", "{}", err.message);
        assert!(
            s.server()
                .expect("server")
                .bans()
                .list()
                .entries
                .iter()
                .any(|e| e.reason == "spam"),
            "the entry is active for this run"
        );
        assert!(
            s.peer_state(PeerId(1)).is_none(),
            "the banned player is removed although the file was not saved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// fixrev2-v048c-merge-audit: a `bans.ron` that was unreadable at start is a read-only store.
    /// `server.ban` documents `ban_store_save_failed` for it (book/agents/net-dedicated-server.md):
    /// the player is removed and the entry applies for this run, but the operator is told it is
    /// not on disk. The corr-net merge had made this a success.
    #[test]
    fn a_ban_on_a_read_only_store_is_reported_as_not_saved() {
        use crate::session::hub::SimInput;
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-ban-ro-kick-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("bans.ron");
        std::fs::write(&path, "not a ban list").expect("bans");
        let mut s = NetSession::default();
        s.set_manual_clock(0);
        s.host_local(
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
        s.add_simulated_clients(2, SimInput::Idle).expect("sims");
        for _ in 0..60 {
            s.advance_manual_clock(16);
            s.update();
        }
        let store = BanStore::open(&path);
        assert!(!store.writable(), "an unreadable file opens read-only");
        s.server_mut().expect("server").set_bans(store);
        assert!(s.peer_state(PeerId(1)).is_some(), "peer 1 joined");
        let err = ban_player(&mut s, "1", None, "spam").expect_err("the ban is not on disk");
        assert_eq!(err.code, "ban_store_save_failed", "{}", err.message);
        assert!(
            s.server()
                .expect("server")
                .bans()
                .list()
                .entries
                .iter()
                .any(|e| e.reason == "spam"),
            "the entry is active for this run"
        );
        assert!(s.peer_state(PeerId(1)).is_none(), "the banned player is removed");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "not a ban list",
            "the unreadable file is not overwritten"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit net1 (platform): the Windows twin of the Unix world-readable check — a token file
    /// whose DACL grants read to Users (or Everyone, or Authenticated Users) leaves remote admin
    /// off; a plain file only the owner can read still loads.
    #[cfg(windows)]
    #[test]
    fn admin_token_readable_by_users_disables_remote_admin() {
        use windows_sys::Win32::Security::{WinAuthenticatedUserSid, WinBuiltinUsersSid, WinWorldSid};
        use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-admin-acl-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        // The plain token below inherits the directory's ACL; make that owner-only here, so the
        // result does not depend on what `%TEMP%` grants (a TEMP that inherits `Users:(RX)`
        // made the first assert fail; audit rev-kimi-17 fixrev-15-k finding 3).
        crate::windows_acl::protect_dir_for_test(&dir).expect("owner-only test dir");
        let path = dir.join("admin.token");
        std::fs::write(&path, "s3cret\n").expect("write");
        assert_eq!(
            read_admin_token_file(&path).as_deref(),
            Some("s3cret"),
            "a plain owner-only token loads"
        );
        // An elevated administrator's token may be owned by `Administrators` (the server-SKU
        // default owner policy); its DACL still names the creating user (audit rev-kimi-17
        // fixrev-15-k finding 1).
        let admin_owned = dir.join("admin-owned.token");
        std::fs::write(&admin_owned, "s3cret\n").expect("write");
        if crate::private_file::set_owner_administrators_for_test(&admin_owned).is_some() {
            assert_eq!(
                read_admin_token_file(&admin_owned).as_deref(),
                Some("s3cret"),
                "an Administrators-owned owner-only token loads"
            );
        } else {
            eprintln!("SKIPPED the Administrators-owner case: this process is not elevated");
        }
        for (sid, who) in [
            (WinBuiltinUsersSid, "Users"),
            (WinWorldSid, "Everyone"),
            (WinAuthenticatedUserSid, "Authenticated Users"),
        ] {
            crate::windows_acl::grant_for_test(&path, sid, FILE_GENERIC_READ)
                .unwrap_or_else(|e| panic!("{who}: grant: {e}"));
            assert_eq!(
                read_admin_token_file(&path),
                None,
                "{who} read disables remote admin"
            );
            std::fs::write(&path, "s3cret\n").expect("rewrite");
            // The grant ACEs accumulate; removing them needs a fresh file.
            std::fs::remove_file(&path).expect("remove");
            std::fs::write(&path, "s3cret\n").expect("fresh file");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Resource, Default)]
    struct CatchupSteps(u32);

    fn count_catchup_steps(mut steps: ResMut<CatchupSteps>) {
        steps.0 += 1;
    }

    /// Spec 76 § 5: an overrun runs further steps up to the game's cap, not a hard-coded 5, and a
    /// cap of 1 forbids catch-up.
    #[test]
    fn an_overrun_catches_up_to_the_games_max_substeps() {
        fn write_atomic(path: &std::path::Path, text: &str) {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, text).expect("write");
            std::fs::rename(&tmp, path).expect("rename");
        }
        fn host_dir() -> std::path::PathBuf {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("regia-net-catchup-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            write_atomic(
                &dir.join("server.toml"),
                r#"
format_version = 1
[server]
name = "Catchup"
port = 7790
max_players = 8
tick_hz = 60
idle_tick_hz = 60
[levels]
rotation = ["levels/a.level.ron"]
[status]
enabled = false
"#,
            );
            dir
        }
        fn steps_after_overrun(max_substeps: u32) -> u32 {
            let dir = host_dir();
            let mut app = App::new();
            app.add_plugins(regia_app::MinimalPlugins)
                .init_resource::<CatchupSteps>()
                .insert_resource({
                    let mut fixed = regia_app::FixedTime::default();
                    fixed.max_substeps = max_substeps;
                    fixed
                })
                .add_systems(regia_app::Update, count_catchup_steps);
            let mut game = AppHostGame::new(app);
            let clock = TestClock::new();
            let mut host = ServerHost::new(
                &dir,
                CliOverrides::default(),
                Box::new(|_| None),
                Box::new(clock.clone()),
            )
            .expect("config");
            host.start(&mut game, HostTransport::Local).expect("start");
            game.app.world_mut().resource_mut::<CatchupSteps>().0 = 0;
            let period = Duration::from_secs_f64(1.0 / 60.0);
            clock.advance(period * 4 + Duration::from_millis(1));
            assert!(host.run_once(&mut game).is_none(), "still running");
            let steps = game.app.world().resource::<CatchupSteps>().0;
            let _ = std::fs::remove_dir_all(&dir);
            steps
        }
        assert_eq!(steps_after_overrun(1), 1, "a cap of 1 is one step");
        assert_eq!(
            steps_after_overrun(5),
            5,
            "four periods behind plus the due step is five"
        );
    }

    /// fixrev6 (ADR-5973): an unban saves `bans.ron`, and the save drops the entries expired at
    /// the `now` it is given. That `now` is the session clock the entries were stamped on: with
    /// the system clock an hour ahead of the session clock, the unban of one entry must not prune
    /// another entry that the session still enforces.
    #[test]
    fn an_unban_prunes_by_the_session_clock() {
        use crate::session::{BanKind, BanStore, HostOptions, NetSession};
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-unban-clock-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut s = NetSession::default();
        s.set_manual_clock(0);
        s.set_unix_base_s(crate::session::unix_now_s() - 3600);
        s.host_local(
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
        s.server_mut()
            .expect("server")
            .set_bans(BanStore::open(&dir.join("bans.ron")));
        let now = s.now_unix_s();
        let kept = s
            .ban(BanKind::Name, "heckler", Some(now + 60), "spam")
            .expect("ban");
        let gone = s
            .ban(BanKind::Name, "troll", Some(now + 60), "spam")
            .expect("ban");
        unban_entry(&mut s, gone.0).expect("unban");
        let ids: Vec<BanId> = s
            .server()
            .expect("server")
            .bans()
            .list()
            .entries
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(
            ids,
            [kept],
            "the other entry is active on the session clock and stays"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Resource, Default)]
    struct SkippedRows(Vec<String>);

    fn record_skipped_row(world: &mut World, event: &HostEvent<'_>) -> Result<(), String> {
        if let HostEvent::NextLevelFailed { map_id, .. } = event {
            world.resource_mut::<SkippedRows>().0.push((*map_id).to_owned());
        }
        Ok(())
    }

    struct EndsNow;

    impl crate::session::SessionRules for EndsNow {
        fn match_over(&self, _world: &World) -> bool {
            true
        }
    }

    /// Counts `server_next_level_failed` warnings on this thread.
    struct FailedLevelLogs(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for FailedLevelLogs {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            struct Message(String);
            impl tracing::field::Visit for Message {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "message" || field.name() == "map_id" {
                        self.0.push_str(&format!("{value:?} "));
                    }
                }
            }
            let mut message = Message(String::new());
            event.record(&mut message);
            if message.0.contains("server_next_level_failed") {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(message.0);
            }
        }
    }

    #[test]
    fn a_missing_level_in_the_rotation_is_skipped_with_server_next_level_failed() {
        use tracing_subscriber::layer::SubscriberExt;
        let dir = regia_test_utils::unique_temp_dir("skip-missing-level");
        regia_test_utils::write_atomic(
            &dir.join("server.toml"),
            br#"
format_version = 4
[server]
name = "Skip"
tick_hz = 60
idle_tick_hz = 60
[levels]
rotation = ["levels/a.level.ron", "levels/missing.level.ron", "levels/b.level.ron"]
match_end_delay_s = 1
[status]
enabled = false
"#,
        )
        .expect("server.toml");
        let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let subscriber = tracing_subscriber::registry().with(FailedLevelLogs(logs.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let mut app = App::new();
            app.add_plugins(regia_app::MinimalPlugins)
                .init_resource::<SkippedRows>()
                .init_resource::<HostGameHooks>()
                .insert_resource({
                    let mut fixed = regia_app::FixedTime::default();
                    fixed.max_substeps = 1;
                    fixed
                });
            app.world_mut()
                .resource_mut::<HostGameHooks>()
                .add_event_hook(record_skipped_row);
            let mut game = AppHostGame::new(app);
            game.session().set_rules(EndsNow);
            game.level_exists = Some(Box::new(|_app, level| !level.contains("missing")));
            let clock = TestClock::new();
            let mut host = ServerHost::new(
                &dir,
                CliOverrides::default(),
                Box::new(|_| None),
                Box::new(clock.clone()),
            )
            .expect("config");
            host.start(&mut game, HostTransport::Local).expect("start");
            assert!(host.run_once(&mut game).is_none());
            assert_eq!(host.level(), "levels/a.level.ron");
            clock.advance(Duration::from_secs(1));
            assert!(host.run_once(&mut game).is_none());
            assert_eq!(host.level(), "levels/b.level.ron", "the missing row is skipped");
            assert_eq!(host.tick(), 2, "one attempt per row, not a hot loop");
            let skipped = game.app.world().resource::<SkippedRows>().0.clone();
            assert_eq!(skipped, vec!["levels/missing.level.ron".to_owned()]);
            let failure = host
                .link()
                .snapshot()
                .rotation
                .last_failure
                .expect("last failure");
            assert_eq!(failure.map_id, "levels/missing.level.ron");
            assert_eq!(failure.error, "level_not_in_pak");
        });
        let logged = logs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            logged.iter().any(|line| {
                line.contains("server_next_level_failed") && line.contains("levels/missing.level.ron")
            }),
            "{logged:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-8897: a `.playlist.ron` row outside its player gate and a row whose level is missing
    /// are passed over, and `server.rotation_get`'s view lists both with their reason.
    #[test]
    fn gated_and_missing_rows_are_listed_in_the_rotation_skips() {
        let dir = regia_test_utils::unique_temp_dir("rotation-skips");
        regia_test_utils::write_atomic(
            &dir.join("server.toml"),
            br#"
format_version = 4
[server]
name = "Skips"
tick_hz = 60
idle_tick_hz = 60
[levels]
playlist = "public.playlist.ron"
match_end_delay_s = 1
[status]
enabled = false
"#,
        )
        .expect("server.toml");
        regia_test_utils::write_atomic(
            &dir.join("public.playlist.ron"),
            br#"(
    format_version: 1,
    looping: true,
    no_repeat_last: 1,
    entries: [
        (map_id: "levels/a.level.ron", mode: ""),
        (map_id: "levels/big.level.ron", mode: "", min_players: 4),
        (map_id: "levels/missing.level.ron", mode: ""),
        (map_id: "levels/b.level.ron", mode: ""),
    ],
)"#,
        )
        .expect("playlist");
        // The host warns `server_next_level_failed` for the missing row. Run under a capturing
        // subscriber, as the test above does, so this test never caches that callsite as
        // uninteresting while the other one waits for the line.
        use tracing_subscriber::layer::SubscriberExt;
        let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let subscriber = tracing_subscriber::registry().with(FailedLevelLogs(logs));
        tracing::subscriber::with_default(subscriber, || {
            let mut app = App::new();
            app.add_plugins(regia_app::MinimalPlugins)
                .init_resource::<SkippedRows>()
                .init_resource::<HostGameHooks>()
                .insert_resource({
                    let mut fixed = regia_app::FixedTime::default();
                    fixed.max_substeps = 1;
                    fixed
                });
            let mut game = AppHostGame::new(app);
            game.session().set_rules(EndsNow);
            game.level_exists = Some(Box::new(|_app, level| !level.contains("missing")));
            let clock = TestClock::new();
            let mut host = ServerHost::new(
                &dir,
                CliOverrides::default(),
                Box::new(|_| None),
                Box::new(clock.clone()),
            )
            .expect("config");
            host.start(&mut game, HostTransport::Local).expect("start");
            assert!(host.run_once(&mut game).is_none());
            assert_eq!(host.level(), "levels/a.level.ron");
            clock.advance(Duration::from_secs(1));
            assert!(host.run_once(&mut game).is_none());
            assert_eq!(
                host.level(),
                "levels/b.level.ron",
                "the gated and the missing row are passed over"
            );
            let view = host.link().snapshot().rotation;
            assert_eq!(view.order, "in_order");
            let skipped: Vec<(u32, &str, &str)> = view
                .skipped
                .iter()
                .map(|skip| (skip.index, skip.map_id.as_str(), skip.reason.as_str()))
                .collect();
            assert_eq!(
                skipped,
                [
                    (1, "levels/big.level.ron", "too_few_players"),
                    (2, "levels/missing.level.ron", "missing_content"),
                ]
            );
            assert_eq!(
                host.playlist.id, "public",
                "the file name is the lobby playlist id"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rule sets a test world hands the host.
    #[derive(Resource)]
    struct TestRuleSets(regia_rules::ruleset::RuleSetLibrary);

    fn test_rule_sets(world: &World) -> Option<regia_rules::ruleset::RuleSetLibrary> {
        world.get_resource::<TestRuleSets>().map(|sets| sets.0.clone())
    }

    fn rules_host(dir: &Path, playlist: &str) -> (AppHostGame, regia_rules::ruleset::RuleSetLibrary) {
        regia_test_utils::write_atomic(
            &dir.join("server.toml"),
            br#"
format_version = 4
[server]
name = "Rules"
tick_hz = 60
idle_tick_hz = 60
[levels]
playlist = "rules.playlist.ron"
match_end_delay_s = 1
[status]
enabled = false
"#,
        )
        .expect("server.toml");
        regia_test_utils::write_atomic(&dir.join("rules.playlist.ron"), playlist.as_bytes())
            .expect("playlist");
        let mut library = regia_rules::ruleset::RuleSetLibrary::new();
        let mut set =
            regia_rules::ruleset::RuleSet::new("ctf_custom", regia_rules::mode::MatchMode::CaptureTheFlag);
        set.options = vec![regia_rules::ruleset::OptionDef {
            key: "caps".into(),
            kind: regia_rules::ruleset::OptionKind::Int,
            default: "3".into(),
            min: Some("1".into()),
            max: Some("10".into()),
            choices: Vec::new(),
            bind: Some("objectives.capture_limit".into()),
        }];
        library.insert(set).expect("rule set");
        let mut app = App::new();
        app.add_plugins(regia_app::MinimalPlugins)
            .insert_resource(TestRuleSets(library.clone()))
            .init_resource::<HostGameHooks>()
            .insert_resource({
                let mut fixed = regia_app::FixedTime::default();
                fixed.max_substeps = 1;
                fixed
            });
        {
            let mut hooks = app.world_mut().resource_mut::<HostGameHooks>();
            hooks.set_rule_sets(test_rule_sets);
            hooks.set_manifest_rules(true);
        }
        let mut game = AppHostGame::new(app);
        game.session().set_rules(EndsNow);
        (game, library)
    }

    /// R-04 (5), ADR-8881: with the game's rule sets, the dedicated host puts the playlist's
    /// combined rules hash in the session manifest before it hosts, passes over a row whose rule
    /// set does not resolve with the row's options (reason `ruleset_option_range`), keeps a row
    /// whose mode names no rule set (a game's own rules), and shows the hash in
    /// `server.rotation_get`'s view.
    #[test]
    fn the_rotation_resolves_rule_sets_and_sets_the_rules_hash() {
        let dir = regia_test_utils::unique_temp_dir("rotation-rules");
        let (mut game, library) = rules_host(
            &dir,
            r#"(
    format_version: 1,
    looping: true,
    entries: [
        (map_id: "levels/a.level.ron", mode: "ctf_custom"),
        (map_id: "levels/b.level.ron", mode: "ctf_custom", options: { "caps": "99" }),
        (map_id: "levels/c.level.ron", mode: "SiegeRules"),
    ],
)"#,
        );
        let mut bad = regia_rules::playlist::PlaylistEntry::new("levels/b.level.ron", "ctf_custom");
        bad.options.insert("caps".into(), "99".into());
        let rows = [
            regia_rules::playlist::PlaylistEntry::new("levels/a.level.ron", "ctf_custom"),
            bad,
            regia_rules::playlist::PlaylistEntry::new("levels/c.level.ron", "SiegeRules"),
        ];
        let expected = library.playlist_rules(&rows).hash;
        assert!(expected.is_some());
        let clock = TestClock::new();
        let mut host = ServerHost::new(
            &dir,
            CliOverrides::default(),
            Box::new(|_| None),
            Box::new(clock.clone()),
        )
        .expect("config");
        host.start(&mut game, HostTransport::Local).expect("start");
        assert_eq!(
            game.session().rules_hash(),
            expected,
            "the manifest carries the rules"
        );
        let manifest = game.session().session_manifest();
        let pack = manifest
            .packs
            .iter()
            .find(|(name, _)| name == crate::session::manifest::RULES_PACK)
            .map(|(_, hash)| *hash);
        assert_eq!(pack, expected, "the handshake manifest names the rules hash");
        assert!(host.run_once(&mut game).is_none());
        assert_eq!(host.level(), "levels/a.level.ron");
        clock.advance(Duration::from_secs(1));
        assert!(host.run_once(&mut game).is_none());
        assert_eq!(
            host.level(),
            "levels/c.level.ron",
            "the unresolved row is passed over; a game-rules row plays"
        );
        let view = host.link().snapshot().rotation;
        assert_eq!(view.rules_hash, expected);
        assert!(view.rules_in_manifest);
        let skipped: Vec<(u32, &str, &str)> = view
            .skipped
            .iter()
            .map(|skip| (skip.index, skip.map_id.as_str(), skip.reason.as_str()))
            .collect();
        assert_eq!(skipped, [(1, "levels/b.level.ron", "ruleset_option_range")]);
        let failure = view.last_failure.expect("the refusal is the last failure");
        assert!(
            failure.error.contains("ruleset_option_range"),
            "{}",
            failure.error
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R-04 (5): without the game's opt-in the host still resolves rows and shows the hash, but
    /// the manifest has no rules pack, so clients that set no rules hash still join.
    #[test]
    fn the_rules_hash_stays_out_of_the_manifest_without_the_opt_in() {
        let dir = regia_test_utils::unique_temp_dir("rotation-rules-off");
        let (mut game, library) = rules_host(
            &dir,
            r#"(
    format_version: 1,
    entries: [(map_id: "levels/a.level.ron", mode: "ctf_custom")],
)"#,
        );
        game.app
            .world_mut()
            .resource_mut::<HostGameHooks>()
            .set_manifest_rules(false);
        let expected = library
            .playlist_rules(&[regia_rules::playlist::PlaylistEntry::new(
                "levels/a.level.ron",
                "ctf_custom",
            )])
            .hash;
        let mut host = ServerHost::new(
            &dir,
            CliOverrides::default(),
            Box::new(|_| None),
            Box::new(TestClock::new()),
        )
        .expect("config");
        host.start(&mut game, HostTransport::Local).expect("start");
        assert_eq!(game.session().rules_hash(), None);
        let view = host.link().snapshot().rotation;
        assert_eq!(view.rules_hash, expected);
        assert!(!view.rules_in_manifest);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R-04 (5): a starting row whose rule set does not resolve stops the start (content error).
    #[test]
    fn a_starting_row_whose_rules_do_not_resolve_stops_the_start() {
        let dir = regia_test_utils::unique_temp_dir("rotation-rules-start");
        let (mut game, _) = rules_host(
            &dir,
            r#"(
    format_version: 1,
    entries: [(map_id: "levels/a.level.ron", mode: "ctf_custom", options: { "caps": "0" })],
)"#,
        );
        let mut host = ServerHost::new(
            &dir,
            CliOverrides::default(),
            Box::new(|_| None),
            Box::new(TestClock::new()),
        )
        .expect("config");
        let error = host.start(&mut game, HostTransport::Local).expect_err("refused");
        assert_eq!(error.exit, ExitCode::Content);
        assert!(
            error
                .lines
                .iter()
                .any(|line| line.contains("ruleset_option_range")),
            "{:?}",
            error.lines
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A weighted cycle may list the current row twice before a later row that fits. Stopping
    /// because the row index equals the current row treats that second copy as a wrap and never
    /// tries the later row. The server then stays on a map its own player gate rejects.
    #[test]
    fn a_weighted_second_copy_of_the_current_row_does_not_hide_the_row_that_fits() {
        let dir = regia_test_utils::unique_temp_dir("rotation-weighted-copy");
        regia_test_utils::write_atomic(
            &dir.join("server.toml"),
            br#"
format_version = 4
[server]
name = "Weighted"
tick_hz = 60
idle_tick_hz = 60
[levels]
playlist = "public.playlist.ron"
match_end_delay_s = 1
[status]
enabled = false
"#,
        )
        .expect("server.toml");
        regia_test_utils::write_atomic(
            &dir.join("public.playlist.ron"),
            br#"(
    format_version: 1,
    looping: true,
    order: weighted(seed: 1),
    entries: [
        (map_id: "levels/a.level.ron", mode: "", weight: 2, min_players: 4),
        (map_id: "levels/b.level.ron", mode: "", weight: 1),
    ],
)"#,
        )
        .expect("playlist");
        let mut app = App::new();
        app.add_plugins(regia_app::MinimalPlugins)
            .init_resource::<HostGameHooks>()
            .insert_resource({
                let mut fixed = regia_app::FixedTime::default();
                fixed.max_substeps = 1;
                fixed
            });
        let mut game = AppHostGame::new(app);
        game.session().set_rules(EndsNow);
        game.level_exists = Some(Box::new(|_app, _level| true));
        let clock = TestClock::new();
        let mut host = ServerHost::new(&dir, CliOverrides::default(), Box::new(|_| None), Box::new(clock))
            .expect("config");
        host.start(&mut game, HostTransport::Local).expect("start");
        assert_eq!(host.level(), "levels/a.level.ron");
        // The cycle a weighted seed can draw: the gated row twice, then the row that fits.
        // No peers are connected, so the player gate rejects row 0 (minimum 4).
        host.playlist.cycle = vec![0, 0, 1];
        host.playlist.index = 0;
        host.playlist.cycle_number = 0;
        host.playlist.recent.clear();
        // rules-review-fixes-b: the review's proof called `expect_err` here, which contradicts its
        // own assertion below (a host that travels to row 1 has succeeded). The travel must
        // succeed AND land on row 1: both are asserted, nothing is dropped.
        let result = host.next_level(&mut game);
        assert_eq!(
            host.level(),
            "levels/b.level.ron",
            "the walk stopped on the second copy of row 0 and never tried row 1 ({result:?})"
        );
        assert!(result.is_ok(), "the travel to row 1 succeeds: {result:?}");
        let skipped: Vec<(u32, &str)> = host
            .rotation_skips
            .iter()
            .map(|skip| (skip.index, skip.reason.as_str()))
            .collect();
        assert_eq!(skipped, [(0, "too_few_players")], "row 0 is listed once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rules-playlist-rotation-review finding 1: a row the player gate rejects was taken with
    /// `take_next` (recorded as played) before the gate ran, so with `no_repeat_last: 2` it sat in
    /// `recent` and the next cycle deferred a map that never played.
    #[test]
    fn a_gated_row_is_not_recorded_as_played() {
        let dir = regia_test_utils::unique_temp_dir("rotation-gated-recent");
        regia_test_utils::write_atomic(
            &dir.join("server.toml"),
            br#"
format_version = 4
[server]
name = "Recent"
tick_hz = 60
idle_tick_hz = 60
[levels]
playlist = "public.playlist.ron"
match_end_delay_s = 1
[status]
enabled = false
"#,
        )
        .expect("server.toml");
        regia_test_utils::write_atomic(
            &dir.join("public.playlist.ron"),
            br#"(
    format_version: 1,
    looping: true,
    order: shuffle(seed: 3),
    no_repeat_last: 2,
    entries: [
        (map_id: "levels/a.level.ron", mode: ""),
        (map_id: "levels/big.level.ron", mode: "", min_players: 4),
        (map_id: "levels/b.level.ron", mode: ""),
    ],
)"#,
        )
        .expect("playlist");
        let mut app = App::new();
        app.add_plugins(regia_app::MinimalPlugins)
            .init_resource::<HostGameHooks>()
            .insert_resource({
                let mut fixed = regia_app::FixedTime::default();
                fixed.max_substeps = 1;
                fixed
            });
        let mut game = AppHostGame::new(app);
        game.session().set_rules(EndsNow);
        game.level_exists = Some(Box::new(|_app, _level| true));
        let clock = TestClock::new();
        let mut host = ServerHost::new(&dir, CliOverrides::default(), Box::new(|_| None), Box::new(clock))
            .expect("config");
        host.start(&mut game, HostTransport::Local).expect("start");
        assert_eq!(host.level(), "levels/a.level.ron");
        // Row 0 plays; the rest of the cycle is the gated row, then row 2.
        host.playlist.cycle = vec![0, 1, 2];
        host.playlist.index = 1;
        host.playlist.cycle_number = 0;
        host.playlist.recent = vec![0];
        let result = host.next_level(&mut game);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(host.level(), "levels/b.level.ron");
        assert!(
            !host.playlist.recent.contains(&1),
            "the gated row never played, so the repeat guard must not hold it back: recent {:?}",
            host.playlist.recent
        );
        assert_eq!(host.playlist.recent, vec![0, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same class as playlist finding 1, on the load path the gate fix does not cover.
    /// `take_next_for` records the chosen row before `change_level`. When that load fails and a
    /// later row in the same call succeeds, the playlist is kept (only a total failure restores
    /// the clone), so with `no_repeat_last: 2` a map that never loaded sits in `recent`.
    #[test]
    fn a_row_that_fails_to_load_is_not_recorded_as_played() {
        let dir = regia_test_utils::unique_temp_dir("rotation-load-failed-recent");
        regia_test_utils::write_atomic(
            &dir.join("server.toml"),
            br#"
format_version = 4
[server]
name = "LoadFailed"
tick_hz = 60
idle_tick_hz = 60
[levels]
playlist = "public.playlist.ron"
match_end_delay_s = 1
[status]
enabled = false
"#,
        )
        .expect("server.toml");
        regia_test_utils::write_atomic(
            &dir.join("public.playlist.ron"),
            br#"(
    format_version: 1,
    looping: true,
    order: shuffle(seed: 3),
    no_repeat_last: 2,
    entries: [
        (map_id: "levels/a.level.ron", mode: ""),
        (map_id: "levels/broken.level.ron", mode: ""),
        (map_id: "levels/b.level.ron", mode: ""),
    ],
)"#,
        )
        .expect("playlist");
        let mut app = App::new();
        app.add_plugins(regia_app::MinimalPlugins)
            .init_resource::<HostGameHooks>()
            .insert_resource({
                let mut fixed = regia_app::FixedTime::default();
                fixed.max_substeps = 1;
                fixed
            });
        let mut game = AppHostGame::new(app);
        game.session().set_rules(EndsNow);
        game.level_exists = Some(Box::new(|_app, _level| true));
        game.load_level = Some(Box::new(|_app, level| {
            if level.contains("broken") {
                Err("load failed".into())
            } else {
                Ok(())
            }
        }));
        let clock = TestClock::new();
        let mut host = ServerHost::new(&dir, CliOverrides::default(), Box::new(|_| None), Box::new(clock))
            .expect("config");
        host.start(&mut game, HostTransport::Local).expect("start");
        assert_eq!(host.level(), "levels/a.level.ron");
        // Row 0 has played. The rest of the cycle is the row whose load fails, then row 2.
        host.playlist.cycle = vec![0, 1, 2];
        host.playlist.index = 1;
        host.playlist.cycle_number = 0;
        host.playlist.recent = vec![0];
        let result = host.next_level(&mut game);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(host.level(), "levels/b.level.ron");
        assert!(
            !host.playlist.recent.contains(&1),
            "the row never loaded, so the repeat guard must not hold it back: recent {:?}",
            host.playlist.recent
        );
        assert_eq!(host.playlist.recent, vec![0, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// rc1-rev-net-k 12 (`docs/04-conventions.md` § 8): the server host's OS-specific code lives in
/// `server_host/platform/`; the shared files hold no `cfg(target_os | unix | windows)` outside
/// their test modules.
#[cfg(test)]
mod platform_seam_tests {
    /// The lines of `src` before its `#[cfg(test)]` test module that select a platform.
    fn platform_cfgs(src: &str) -> Vec<String> {
        src.lines()
            .take_while(|l| l.trim_end() != "#[cfg(test)]")
            .enumerate()
            .filter(|(_, l)| {
                let l = l.trim_start();
                (l.starts_with("#[cfg") || l.starts_with("#![cfg") || l.contains("cfg!("))
                    && (l.contains("target_os") || l.contains("unix") || l.contains("windows"))
            })
            .map(|(i, l)| format!("{}: {}", i + 1, l.trim()))
            .collect()
    }

    #[test]
    fn platform_code_stays_behind_the_seam() {
        for (name, src) in [
            ("crash.rs", include_str!("crash.rs")),
            ("signals.rs", include_str!("signals.rs")),
            ("mod.rs", include_str!("mod.rs")),
            ("ecs_host.rs", include_str!("ecs_host.rs")),
        ] {
            let found = platform_cfgs(src);
            assert!(
                found.is_empty(),
                "server_host/{name}: platform cfg outside platform/: {found:?}"
            );
        }
    }
}

#[cfg(test)]
mod port_map_switch_tests {
    use super::*;

    /// N-19 (ADR-10069): `--no-port-map` turns the router mapping off for a remote relay; the host
    /// check names why. A relay on this machine never maps.
    #[test]
    fn no_port_map_turns_the_mapping_off_for_a_remote_relay() {
        let remote: SocketAddr = "203.0.113.9:7800".parse().expect("addr");
        let local: SocketAddr = "127.0.0.1:7800".parse().expect("addr");
        let on = CliOverrides::parse(&["--relay".into(), remote.to_string()]).expect("cli");
        let off = CliOverrides::parse(&["--relay".into(), remote.to_string(), "--no-port-map".into()])
            .expect("cli");
        assert!(off.no_port_map && !on.no_port_map);
        assert_eq!(port_map_choice(remote, &on), Ok(()));
        assert_eq!(port_map_choice(remote, &off), Err("port_map_disabled"));
        assert_eq!(port_map_choice(local, &on), Err("port_map_off"));
        assert_eq!(port_map_choice(local, &off), Err("port_map_off"));
    }

    /// ADR-10399: `--relay-only` is a flag. The default registrar still publishes its address.
    #[test]
    fn relay_only_is_a_cli_flag() {
        let on = CliOverrides::parse(&["--relay-only".into()]).expect("cli");
        assert!(on.relay_only);
        let off = CliOverrides::parse(&["--relay".into(), "127.0.0.1:7800".into()]).expect("cli");
        assert!(!off.relay_only);
        assert!(!CliOverrides::default().relay_only);
    }
}
