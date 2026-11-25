// SPDX-License-Identifier: MPL-2.0
//! `server.toml` and its overrides (spec 76 § 4).
//!
//! Precedence: built-in defaults < `server.toml` < environment variables < command line. Unknown
//! keys warn (one line each) and are ignored, so newer files run on older servers. Validation
//! failures are reported one line per field with the expected range; the host exits with code 2.
//! `format_version = 4` adds timeout policy (ADR-7950), the optional `[net] mtu_probing` (ADR-8300) and the optional
//! `levels.playlist` path (ADR-8345), all folded into the unreleased v4 per ADR-7724 (ADR-8529); v2 adds persistence
//! and v3 bots/entropy. Older files still load with missing sections defaulted (persistence and bots off), and
//! [`migrate_server_toml`] rewrites them for `regia upgrade-project`.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::{Resource, bevy_ecs};
use serde::Serialize;

/// The `format_version` this build writes (v4: timeout policy, ADR-7950, path MTU probing,
/// ADR-8300, and the optional `levels.playlist`, ADR-8345).
pub const SERVER_TOML_FORMAT_VERSION: u32 = 4;

/// The oldest `format_version` this build still reads (v1: no `[persistence]` section).
pub const SERVER_TOML_MIN_FORMAT_VERSION: u32 = 1;

/// Who a returning player is, for per-player persistent state (`[persistence] player_identity`,
/// ADR-3354).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerIdentityMode {
    /// The authenticator's platform id (`steam:<id>`, spec 77 § 4) only: a player without one has
    /// no persistent state.
    #[default]
    Platform,
    /// The platform id, else the player's name (`name:<name>`). Anyone may join under any name:
    /// only for password-protected or LAN servers.
    PlatformOrName,
}

impl PlayerIdentityMode {
    /// The `server.toml` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::PlatformOrName => "platform_or_name",
        }
    }

    /// Parses the `server.toml` spelling.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "platform" => Some(Self::Platform),
            "platform_or_name" => Some(Self::PlatformOrName),
            _ => None,
        }
    }
}

/// `[persistence]`: the world a dedicated server keeps across restarts (ADR-3354). Read by
/// `regia_framework::server_persistence`; the host itself only passes it on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersistenceSettings {
    /// `persistence.enabled` (default off: a match server starts every level fresh).
    pub enabled: bool,
    /// `persistence.dir`: where the saves live, relative to the server directory.
    pub dir: String,
    /// `persistence.autosave_ticks`: an autosave every this many host ticks (0 = no autosave).
    pub autosave_ticks: u32,
    /// `persistence.keep`: saves kept per level (the oldest beyond it are removed).
    pub keep: u32,
    /// `persistence.save_on_shutdown`: save after the drain of a graceful shutdown.
    pub save_on_shutdown: bool,
    /// `persistence.save_on_level_change`: save the level being left before a travel.
    pub save_on_level_change: bool,
    /// `persistence.player_identity`: how a returning player is recognised.
    pub player_identity: PlayerIdentityMode,
}

impl Default for PersistenceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: "saves".into(),
            autosave_ticks: 18_000,
            keep: 5,
            save_on_shutdown: true,
            save_on_level_change: true,
            player_identity: PlayerIdentityMode::Platform,
        }
    }
}

/// The largest `persistence.keep`.
pub const MAX_PERSISTENCE_KEEP: u32 = 1000;

/// Environment variables read as overrides.
pub const SERVER_ENV_VARS: &[&str] = &[
    "REGIA_SERVER_PORT",
    "REGIA_SERVER_MAX_PLAYERS",
    "REGIA_SERVER_LEVEL",
    "REGIA_SERVER_PASSWORD",
];

/// `[bots]` in `server.toml`: connectionless server participants (ADR-3346).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ServerBotSettings {
    /// Desired humans plus bots in a match.
    pub min_players: u32,
    /// Maximum filler and reconnect-takeover bots (0 disables bots).
    pub max_bots: u32,
    /// Stable ticks before a human join replaces a bot.
    pub join_delay_ticks: u32,
    /// Stable ticks before a missing filler bot is added.
    pub leave_delay_ticks: u32,
    /// Seed for deterministic perception, patrol and aim noise.
    pub seed: u64,
    /// Decision delay in fixed ticks.
    pub reaction_ticks: u32,
    /// Maximum absolute seeded aim error in radians.
    pub aim_error_radians: f32,
    /// Bot sight radius in metres.
    pub perception_radius: f32,
}

impl Default for ServerBotSettings {
    fn default() -> Self {
        Self {
            min_players: 0,
            max_bots: 0,
            join_delay_ticks: 30,
            leave_delay_ticks: 30,
            seed: 0,
            reaction_ticks: 12,
            aim_error_radians: 0.03,
            perception_radius: 40.0,
        }
    }
}

/// The resolved server configuration.
#[derive(Resource, Debug, Clone, PartialEq, Serialize)]
pub struct ServerSettings {
    /// `[net]` interruption, silence timeout and startup grace in milliseconds (v4).
    pub timeout: crate::session::TimeoutPolicy,
    /// `[net] mtu_probing` (v4, optional): path MTU discovery on the game port's links (spec 120 § 4.3,
    /// ADR-8300). Off by default.
    pub mtu_probing: bool,
    /// `format_version`.
    pub format_version: u32,
    /// `[server] name`.
    pub name: String,
    /// `[server] bind`.
    pub bind: IpAddr,
    /// `[server] port`.
    pub port: u32,
    /// `[server] max_players`.
    pub max_players: u32,
    /// `[server] password` (empty = none).
    pub password: String,
    /// `[server] tick_hz`.
    pub tick_hz: u32,
    /// `[server] motd`.
    pub motd: String,
    /// `[server] idle_tick_hz`.
    pub idle_tick_hz: u32,
    /// `[levels] rotation`.
    pub rotation: Vec<String>,
    /// `[levels] playlist`: a `.playlist.ron` path inside the server directory. Exclusive with
    /// [`Self::rotation`]. Empty means the rotation list is the playlist (map-only rows).
    pub playlist: String,
    /// `[levels] start_index`.
    pub start_index: usize,
    /// `[levels] match_end_delay_s`.
    pub match_end_delay_s: f64,
    /// `[net] client_rate_bytes`. `None` when `server.toml` does not name it: the project's
    /// `config/net.ron` value stays (spec 76 § 4 precedence, ADR-7213).
    pub client_rate_bytes: Option<u32>,
    /// `[net] server_rate_bytes` (0 = no total cap). `None` when not named (`config/net.ron`
    /// decides).
    pub server_rate_bytes: Option<u32>,
    /// `[net] send_every_ticks`. `None` when not named (`config/net.ron` decides).
    pub send_every_ticks: Option<u32>,
    /// `[net] entropy`: `off` or `range` (spec 72 § 18, ADR-3315). Absent = the project's
    /// `config/net.ron` decides.
    pub entropy: Option<crate::entropy::EntropyMode>,
    /// `[net] encryption`: `auto`, `required` or `off` (ADR-1901).
    pub encryption: crate::crypto::CryptoMode,
    /// `[net] identity_file`: the server's key, relative to the server directory. Created on first
    /// start.
    pub identity_file: String,
    /// `--shared-port`: a replication game (`HostedLink`) serves the session on its link's port
    /// too (ADR-2665 (6)). Off by default: the session then runs local. A command-line switch,
    /// not a `server.toml` key (the v1 file shape stays frozen), so it is not serialized.
    #[serde(skip)]
    pub shared_port: bool,
    /// `[admin] token_file` (relative to the server directory; empty = remote admin off).
    pub admin_token_file: String,
    /// `[status] enabled`.
    pub status_enabled: bool,
    /// `[status] bind`.
    pub status_bind: IpAddr,
    /// `[status] port`.
    pub status_port: u32,
    /// `[logs] dir`.
    pub log_dir: String,
    /// `[logs] keep_files`.
    pub keep_log_files: u32,
    /// `--console`: read commands from stdin even when it is not a terminal.
    pub console: bool,
    /// `[persistence]` (format v2, ADR-3354).
    pub persistence: PersistenceSettings,
    /// `[bots]`: connectionless server participants.
    pub bots: ServerBotSettings,
    /// `[lockstep]` (v4, optional; ADR-8558): the server hosts a lockstep match
    /// ([`crate::server_host::lockstep_game`]). Off by default.
    pub lockstep: crate::lockstep::server::LockstepServerSettings,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            format_version: SERVER_TOML_FORMAT_VERSION,
            timeout: crate::session::TimeoutPolicy::default(),
            mtu_probing: false,
            name: "Regia server".into(),
            bind: IpAddr::from([0, 0, 0, 0]),
            port: 7777,
            max_players: 16,
            password: String::new(),
            tick_hz: 60,
            motd: String::new(),
            idle_tick_hz: 10,
            rotation: Vec::new(),
            playlist: String::new(),
            start_index: 0,
            match_end_delay_s: 10.0,
            client_rate_bytes: None,
            server_rate_bytes: None,
            send_every_ticks: None,
            entropy: None,
            encryption: crate::crypto::CryptoMode::default(),
            identity_file: "server.key".into(),
            shared_port: false,
            admin_token_file: "admin.token".into(),
            status_enabled: false,
            status_bind: IpAddr::from([127, 0, 0, 1]),
            status_port: 7778,
            log_dir: "logs".into(),
            keep_log_files: 10,
            console: false,
            persistence: PersistenceSettings::default(),
            bots: ServerBotSettings::default(),
            lockstep: crate::lockstep::server::LockstepServerSettings::default(),
        }
    }
}

/// One field-level configuration error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    /// `section.key`, `--flag` or the environment variable.
    pub field: String,
    /// What was expected.
    pub expected: String,
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: expected {}", self.field, self.expected)
    }
}

/// The result of resolving a configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConfig {
    /// The settings.
    pub settings: ServerSettings,
    /// The file it came from, if any.
    pub path: Option<PathBuf>,
    /// Unknown keys and other warnings (one line each).
    pub warnings: Vec<String>,
}

/// The command-line overrides of spec 76 § 4.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOverrides {
    /// `--config PATH`.
    pub config: Option<PathBuf>,
    /// `--port N`.
    pub port: Option<String>,
    /// `--bind ADDR`.
    pub bind: Option<String>,
    /// `--max-players N`.
    pub max_players: Option<String>,
    /// `--level PATH` (replaces the rotation with one level).
    pub level: Option<String>,
    /// `--name TEXT`.
    pub name: Option<String>,
    /// `--log-dir DIR`.
    pub log_dir: Option<String>,
    /// `--console`.
    pub console: bool,
    /// `--agent` (dev builds): start the agent protocol server.
    pub agent: bool,
    /// `--relay ADDR|local|local:PORT` (ADR-2440): register with this rendezvous relay for a join
    /// code; `local` also runs a relay inside the host on `127.0.0.1` (dev runs, tests).
    pub relay: Option<String>,
    /// `--shared-port` (ADR-2665 (6)): one game port for the link and the session.
    pub shared_port: bool,
    /// `--no-port-map` (N-19, ADR-10069): with `--relay HOST:PORT`, do not ask the router for a
    /// port mapping (UPnP-IGD, NAT-PMP, PCP); the host check reports `port_map_disabled`. A
    /// `server.toml` key for the same switch waits for the v0.7 format batch.
    pub no_port_map: bool,
    /// `--relay-only` (ADR-10399): with `--relay`, register the game port but do not publish
    /// this host's address. The registrar calls `RelayRegistrar::relay_only`. The router
    /// mapping and the host check are not started. A `server.toml` key waits for the v0.7
    /// format batch.
    pub relay_only: bool,
    /// `--bots N`: fill to N participants and cap bots at N (the net_game server exposes this flag).
    pub bots: Option<String>,
    /// `--upgrade-config` (spec 76 § 4, ADR-5650): rewrite the config file to the current
    /// `format_version` ([`upgrade_config_file`]) and exit instead of starting.
    pub upgrade_config: bool,
}

impl CliOverrides {
    /// Parses the host's arguments (without the program name).
    pub fn parse(args: &[String]) -> Result<Self, Vec<FieldError>> {
        let mut out = Self::default();
        let mut errors = Vec::new();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
                _ => (arg.as_str(), None),
            };
            let mut value = |name: &str| -> Option<String> {
                let v = inline.clone().or_else(|| it.next().cloned());
                if v.is_none() {
                    errors.push(FieldError {
                        field: name.to_owned(),
                        expected: "a value".into(),
                    });
                }
                v
            };
            match flag {
                "--config" => out.config = value("--config").map(PathBuf::from),
                "--port" => out.port = value("--port"),
                "--bind" => out.bind = value("--bind"),
                "--max-players" => out.max_players = value("--max-players"),
                "--level" => out.level = value("--level"),
                "--name" => out.name = value("--name"),
                "--log-dir" => out.log_dir = value("--log-dir"),
                "--console" => out.console = true,
                "--agent" => out.agent = true,
                "--relay" => out.relay = value("--relay"),
                "--shared-port" => out.shared_port = true,
                "--no-port-map" => out.no_port_map = true,
                "--relay-only" => out.relay_only = true,
                "--bots" => out.bots = value("--bots"),
                "--upgrade-config" => out.upgrade_config = true,
                other => errors.push(FieldError {
                    field: other.to_owned(),
                    expected: "one of --config, --port, --bind, --max-players, --level, --name, --log-dir, --console, --agent, --relay, --shared-port, --no-port-map, --relay-only, --bots, --upgrade-config".into(),
                }),
            }
        }
        if errors.is_empty() { Ok(out) } else { Err(errors) }
    }
}

fn type_name(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

struct Apply<'a> {
    errors: &'a mut Vec<FieldError>,
    warnings: &'a mut Vec<String>,
}

impl Apply<'_> {
    fn bad(&mut self, field: &str, expected: &str) {
        self.errors.push(FieldError {
            field: field.to_owned(),
            expected: expected.to_owned(),
        });
    }

    fn string(&mut self, field: &str, v: &toml::Value, out: &mut String) {
        match v.as_str() {
            Some(s) => *out = s.to_owned(),
            None => self.bad(field, &format!("a string, found {}", type_name(v))),
        }
    }

    fn int(&mut self, field: &str, v: &toml::Value) -> Option<i64> {
        match v.as_integer() {
            Some(i) => Some(i),
            None => {
                self.bad(field, &format!("an integer, found {}", type_name(v)));
                None
            }
        }
    }

    fn u32(&mut self, field: &str, v: &toml::Value, out: &mut u32) {
        if let Some(i) = self.int(field, v) {
            match u32::try_from(i) {
                Ok(n) => *out = n,
                Err(_) => self.bad(field, "a non-negative integer"),
            }
        }
    }

    /// A `u32` key whose presence matters: a valid value is `Some`, a bad one leaves `out` alone.
    fn opt_u32(&mut self, field: &str, v: &toml::Value, out: &mut Option<u32>) {
        let mut n = 0;
        let before = self.errors.len();
        self.u32(field, v, &mut n);
        if self.errors.len() == before {
            *out = Some(n);
        }
    }

    fn f32(&mut self, field: &str, v: &toml::Value, out: &mut f32) {
        match v.as_float().or_else(|| v.as_integer().map(|i| i as f64)) {
            Some(n) if n.is_finite() && n >= 0.0 && n <= f64::from(f32::MAX) => *out = n as f32,
            _ => self.bad(field, "a finite non-negative number"),
        }
    }

    fn ip(&mut self, field: &str, v: &toml::Value, out: &mut IpAddr) {
        match v.as_str().and_then(|s| s.parse().ok()) {
            Some(ip) => *out = ip,
            None => self.bad(field, "an IP address such as \"0.0.0.0\""),
        }
    }
}

impl ServerSettings {
    /// Parses `server.toml` text over the built-in defaults and validates the result, with no environment or
    /// command-line overrides.
    pub fn from_toml(text: &str) -> Result<Self, Vec<FieldError>> {
        let mut settings = Self::default();
        let mut errors = Vec::new();
        settings.apply_file(text, &mut errors, &mut Vec::new());
        if errors.is_empty() {
            errors = settings.validate();
        }
        if errors.is_empty() {
            Ok(settings)
        } else {
            Err(errors)
        }
    }

    /// Applies a parsed `server.toml` on top of these settings.
    pub(crate) fn apply_file(
        &mut self,
        text: &str,
        errors: &mut Vec<FieldError>,
        warnings: &mut Vec<String>,
    ) {
        let table: toml::Table = match text.parse() {
            Ok(t) => t,
            Err(e) => {
                errors.push(FieldError {
                    field: "server.toml".into(),
                    expected: format!("valid TOML ({})", e.message()),
                });
                return;
            }
        };
        let mut a = Apply { errors, warnings };
        for (key, value) in &table {
            match (key.as_str(), value) {
                ("format_version", v) => {
                    if let Some(i) = a.int("format_version", v) {
                        match u32::try_from(i) {
                            Ok(v) if (SERVER_TOML_MIN_FORMAT_VERSION..=SERVER_TOML_FORMAT_VERSION)
                                .contains(&v) =>
                            {
                                self.format_version = v;
                            }
                            _ => a.bad(
                                "format_version",
                                &format!(
                                    "{SERVER_TOML_MIN_FORMAT_VERSION}..={SERVER_TOML_FORMAT_VERSION}, found {i}"
                                ),
                            ),
                        }
                    }
                }
                (
                    section @ ("server" | "levels" | "net" | "admin" | "status" | "logs" | "persistence"
                    | "bots" | "lockstep"),
                    toml::Value::Table(t),
                ) => {
                    for (k, v) in t {
                        self.apply_key(section, k, v, &mut a);
                    }
                }
                (section, v)
                    if matches!(
                        section,
                        "server"
                            | "levels"
                            | "net"
                            | "admin"
                            | "status"
                            | "logs"
                            | "persistence"
                            | "bots"
                            | "lockstep"
                    ) =>
                {
                    a.bad(section, &format!("a [{section}] table, found {}", type_name(v)));
                }
                (other, _) => a
                    .warnings
                    .push(format!("server.toml: unknown key `{other}` ignored")),
            }
        }
    }

    fn apply_key(&mut self, section: &str, key: &str, v: &toml::Value, a: &mut Apply<'_>) {
        let field = format!("{section}.{key}");
        let f = field.as_str();
        match (section, key) {
            ("server", "name") => a.string(f, v, &mut self.name),
            ("server", "bind") => a.ip(f, v, &mut self.bind),
            ("server", "port") => a.u32(f, v, &mut self.port),
            ("server", "max_players") => a.u32(f, v, &mut self.max_players),
            ("server", "password") => a.string(f, v, &mut self.password),
            ("server", "tick_hz") => a.u32(f, v, &mut self.tick_hz),
            ("server", "motd") => a.string(f, v, &mut self.motd),
            ("server", "idle_tick_hz") => a.u32(f, v, &mut self.idle_tick_hz),
            ("levels", "rotation") => match v.as_array() {
                Some(list) if list.iter().all(toml::Value::is_str) => {
                    self.rotation = list
                        .iter()
                        .filter_map(|l| l.as_str().map(str::to_owned))
                        .collect();
                }
                _ => a.bad(f, "an array of level paths"),
            },
            ("levels", "playlist") => a.string(f, v, &mut self.playlist),
            ("levels", "start_index") => {
                if let Some(i) = a.int(f, v) {
                    match usize::try_from(i) {
                        Ok(n) => self.start_index = n,
                        Err(_) => a.bad(f, "a non-negative integer"),
                    }
                }
            }
            ("levels", "match_end_delay_s") => {
                match v.as_float().or_else(|| v.as_integer().map(|i| i as f64)) {
                    Some(s) if s >= 0.0 && s.is_finite() => self.match_end_delay_s = s,
                    _ => a.bad(f, "a number of seconds >= 0"),
                }
            }
            ("net", "interrupt_ms" | "timeout_ms" | "start_grace_ms") => {
                if let Some(i) = a.int(f, v) {
                    match u64::try_from(i) {
                        Ok(n) => match key {
                            "interrupt_ms" => self.timeout.interrupt_ms = n,
                            "timeout_ms" => self.timeout.timeout_ms = n,
                            _ => self.timeout.start_grace_ms = n,
                        },
                        Err(_) => a.bad(f, "a non-negative millisecond duration"),
                    }
                }
            }
            ("net", "mtu_probing") => match v.as_bool() {
                Some(b) => self.mtu_probing = b,
                None => a.bad(f, "true or false"),
            },
            ("net", "client_rate_bytes") => a.opt_u32(f, v, &mut self.client_rate_bytes),
            ("net", "server_rate_bytes") => a.opt_u32(f, v, &mut self.server_rate_bytes),
            ("net", "send_every_ticks") => a.opt_u32(f, v, &mut self.send_every_ticks),
            ("net", "entropy") => match v.as_str() {
                Some("off") => self.entropy = Some(crate::entropy::EntropyMode::Off),
                Some("range") => self.entropy = Some(crate::entropy::EntropyMode::Range),
                _ => a.bad(f, "\"off\" or \"range\""),
            },
            ("net", "encryption") => match v.as_str().and_then(crate::crypto::CryptoMode::parse) {
                Some(mode) => self.encryption = mode,
                None => a.bad(f, "\"auto\", \"required\" or \"off\""),
            },
            ("net", "identity_file") => a.string(f, v, &mut self.identity_file),
            ("admin", "token_file") => a.string(f, v, &mut self.admin_token_file),
            ("status", "enabled") => match v.as_bool() {
                Some(b) => self.status_enabled = b,
                None => a.bad(f, "true or false"),
            },
            ("status", "bind") => a.ip(f, v, &mut self.status_bind),
            ("status", "port") => a.u32(f, v, &mut self.status_port),
            ("logs", "dir") => a.string(f, v, &mut self.log_dir),
            ("logs", "keep_files") => a.u32(f, v, &mut self.keep_log_files),
            ("persistence", "enabled") => match v.as_bool() {
                Some(b) => self.persistence.enabled = b,
                None => a.bad(f, "true or false"),
            },
            ("persistence", "dir") => a.string(f, v, &mut self.persistence.dir),
            ("persistence", "autosave_ticks") => a.u32(f, v, &mut self.persistence.autosave_ticks),
            ("persistence", "keep") => a.u32(f, v, &mut self.persistence.keep),
            ("persistence", "save_on_shutdown") => match v.as_bool() {
                Some(b) => self.persistence.save_on_shutdown = b,
                None => a.bad(f, "true or false"),
            },
            ("persistence", "save_on_level_change") => match v.as_bool() {
                Some(b) => self.persistence.save_on_level_change = b,
                None => a.bad(f, "true or false"),
            },
            ("persistence", "player_identity") => match v.as_str().and_then(PlayerIdentityMode::parse) {
                Some(mode) => self.persistence.player_identity = mode,
                None => a.bad(f, "\"platform\" or \"platform_or_name\""),
            },
            ("lockstep", key) => match self.lockstep.apply_key(key, v) {
                Ok(true) => {}
                Ok(false) => a
                    .warnings
                    .push(format!("server.toml: unknown key `{field}` ignored")),
                Err(e) => a.bad(&e.field, &e.expected),
            },
            ("bots", "min_players") => a.u32(f, v, &mut self.bots.min_players),
            ("bots", "max_bots") => a.u32(f, v, &mut self.bots.max_bots),
            ("bots", "join_delay_ticks") => a.u32(f, v, &mut self.bots.join_delay_ticks),
            ("bots", "leave_delay_ticks") => a.u32(f, v, &mut self.bots.leave_delay_ticks),
            ("bots", "reaction_ticks") => a.u32(f, v, &mut self.bots.reaction_ticks),
            ("bots", "seed") => {
                if let Some(i) = a.int(f, v) {
                    match u64::try_from(i) {
                        Ok(n) => self.bots.seed = n,
                        Err(_) => a.bad(f, "a non-negative integer"),
                    }
                }
            }
            ("bots", "aim_error_radians") => a.f32(f, v, &mut self.bots.aim_error_radians),
            ("bots", "perception_radius") => a.f32(f, v, &mut self.bots.perception_radius),
            _ => a
                .warnings
                .push(format!("server.toml: unknown key `{field}` ignored")),
        }
    }

    fn apply_overrides(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        cli: &CliOverrides,
        errors: &mut Vec<FieldError>,
    ) {
        let mut num = |field: &str, text: &str, out: &mut u32| match text.trim().parse::<u32>() {
            Ok(n) => *out = n,
            Err(_) => errors.push(FieldError {
                field: field.to_owned(),
                expected: "a non-negative integer".into(),
            }),
        };
        if let Some(v) = env("REGIA_SERVER_PORT") {
            num("REGIA_SERVER_PORT", &v, &mut self.port);
        }
        if let Some(v) = env("REGIA_SERVER_MAX_PLAYERS") {
            num("REGIA_SERVER_MAX_PLAYERS", &v, &mut self.max_players);
        }
        if let Some(v) = env("REGIA_SERVER_LEVEL") {
            self.rotation = vec![v];
            self.playlist.clear();
            self.start_index = 0;
        }
        if let Some(v) = env("REGIA_SERVER_PASSWORD") {
            self.password = v;
        }
        if let Some(v) = &cli.port {
            num("--port", v, &mut self.port);
        }
        if let Some(v) = &cli.max_players {
            num("--max-players", v, &mut self.max_players);
        }
        if let Some(v) = &cli.bots {
            match v.trim().parse::<u32>() {
                Ok(n) => {
                    self.bots.min_players = n;
                    self.bots.max_bots = n;
                }
                Err(_) => errors.push(FieldError {
                    field: "--bots".into(),
                    expected: "a non-negative integer".into(),
                }),
            }
        }
        if let Some(v) = &cli.bind {
            match v.parse() {
                Ok(ip) => self.bind = ip,
                Err(_) => errors.push(FieldError {
                    field: "--bind".into(),
                    expected: "an IP address".into(),
                }),
            }
        }
        if let Some(v) = &cli.level {
            self.rotation = vec![v.clone()];
            self.playlist.clear();
            self.start_index = 0;
        }
        if let Some(v) = &cli.name {
            self.name.clone_from(v);
        }
        if let Some(v) = &cli.log_dir {
            self.log_dir.clone_from(v);
        }
        self.console |= cli.console;
        self.shared_port |= cli.shared_port;
    }

    /// The validation rules of spec 76 § 4, one error per failing field.
    pub fn validate(&self) -> Vec<FieldError> {
        let mut e = Vec::new();
        let mut bad = |field: &str, expected: &str| {
            e.push(FieldError {
                field: field.to_owned(),
                expected: expected.to_owned(),
            });
        };
        if !(SERVER_TOML_MIN_FORMAT_VERSION..=SERVER_TOML_FORMAT_VERSION).contains(&self.format_version) {
            bad(
                "format_version",
                &format!("{SERVER_TOML_MIN_FORMAT_VERSION}..={SERVER_TOML_FORMAT_VERSION}"),
            );
        }
        if !(1..=65_535).contains(&self.port) {
            bad("server.port", "1..=65535");
        } else if self.port == self.status_port {
            bad("server.port", "a port different from status.port");
        }
        if !(1..=256).contains(&self.max_players) {
            bad("server.max_players", "1..=256");
        }
        // Both are relative to the server directory (spec 76 § 4): one edited line must not make
        // the host read or create files anywhere the service user can reach.
        if !inside_server_dir(&self.admin_token_file) {
            bad("admin.token_file", "a path inside the server directory");
        }
        // The private key too: `Path::join` replaces the base on an absolute path (and, on
        // Windows, on a drive prefix), so an escaping identity file would be read or created
        // outside the server directory (audit net1, platform).
        if !inside_server_dir(&self.identity_file) || self.identity_file.trim().is_empty() {
            bad(
                "net.identity_file",
                "a non-empty path inside the server directory",
            );
        }
        if !inside_server_dir(&self.log_dir) {
            bad("logs.dir", "a path inside the server directory");
        }
        if !inside_server_dir(&self.persistence.dir) || self.persistence.dir.trim().is_empty() {
            bad("persistence.dir", "a non-empty path inside the server directory");
        }
        if !(1..=MAX_PERSISTENCE_KEEP).contains(&self.persistence.keep) {
            bad("persistence.keep", &format!("1..={MAX_PERSISTENCE_KEEP}"));
        }
        if !(10..=240).contains(&self.tick_hz) {
            bad("server.tick_hz", "10..=240");
        }
        if !(1..=240).contains(&self.idle_tick_hz) {
            bad("server.idle_tick_hz", "1..=240");
        }
        if self.send_every_ticks.is_some_and(|n| n < 1) {
            bad("net.send_every_ticks", ">= 1");
        }
        // Silence policy (ADR-7950): `timeout_ms = 20` (seconds by mistake) would drop every peer
        // at once, `interrupt_ms = 0` would interrupt and restore every tick.
        let policy = self.timeout;
        let (min_timeout, min_interrupt, max_grace) = (
            crate::session::TimeoutPolicy::MIN_TIMEOUT_MS,
            crate::session::TimeoutPolicy::MIN_INTERRUPT_MS,
            crate::session::TimeoutPolicy::MAX_START_GRACE_MS,
        );
        if policy.timeout_ms < min_timeout {
            bad("net.timeout_ms", &format!(">= {min_timeout} milliseconds"));
        }
        if policy.interrupt_ms < min_interrupt || policy.interrupt_ms >= policy.timeout_ms {
            bad(
                "net.interrupt_ms",
                &format!(">= {min_interrupt} milliseconds and below net.timeout_ms"),
            );
        }
        if policy.start_grace_ms > max_grace {
            bad("net.start_grace_ms", &format!("0..={max_grace} milliseconds"));
        }
        if self.bots.max_bots > 255 {
            bad("bots.max_bots", "0..=255");
        }
        if self.bots.min_players > self.max_players {
            bad("bots.min_players", "<= server.max_players");
        }
        if self.bots.aim_error_radians > std::f32::consts::PI {
            bad("bots.aim_error_radians", "0..=PI radians");
        }
        let has_rotation = !self.rotation.is_empty();
        let has_playlist = !self.playlist.is_empty();
        if has_rotation && has_playlist {
            bad("levels.playlist", "exclusive with levels.rotation");
        } else if !has_rotation && !has_playlist {
            bad("levels.rotation", "at least one level");
        } else if has_rotation && self.start_index >= self.rotation.len() {
            bad("levels.start_index", &format!("0..{}", self.rotation.len()));
        }
        if has_playlist && !inside_server_dir(&self.playlist) {
            bad("levels.playlist", "a path inside the server directory");
        }
        // These travel in `Welcome` / `TravelBegin` fields with byte limits.
        if self.name.len() > 255 {
            bad("server.name", "at most 255 bytes");
        }
        if self.motd.len() > 4096 {
            bad("server.motd", "at most 4096 bytes");
        }
        if self.rotation.iter().any(|l| l.len() > 255) {
            bad("levels.rotation", "level paths of at most 255 bytes");
        }
        if self.status_enabled && self.status_port > 65_535 {
            bad("status.port", "0..=65535 (0 = any free port)");
        }
        for err in self.lockstep.validate() {
            bad(&err.field, &err.expected);
        }
        e
    }

    /// The game port's socket address.
    pub fn game_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.bind, self.port as u16)
    }

    /// The health endpoint's socket address.
    pub fn status_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.status_bind, self.status_port as u16)
    }

    /// Keys whose change needs a restart (everything a reload does not apply).
    pub fn restart_only_changes(&self, new: &ServerSettings) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut check = |changed: bool, key: &'static str| {
            if changed {
                out.push(key);
            }
        };
        check(self.bind != new.bind, "server.bind");
        check(self.port != new.port, "server.port");
        check(self.tick_hz != new.tick_hz, "server.tick_hz");
        check(self.idle_tick_hz != new.idle_tick_hz, "server.idle_tick_hz");
        check(self.start_index != new.start_index, "levels.start_index");
        check(
            self.match_end_delay_s != new.match_end_delay_s,
            "levels.match_end_delay_s",
        );
        check(self.encryption != new.encryption, "net.encryption");
        check(self.mtu_probing != new.mtu_probing, "net.mtu_probing");
        check(self.identity_file != new.identity_file, "net.identity_file");
        check(
            self.timeout.interrupt_ms != new.timeout.interrupt_ms,
            "net.interrupt_ms",
        );
        check(
            self.timeout.timeout_ms != new.timeout.timeout_ms,
            "net.timeout_ms",
        );
        check(
            self.timeout.start_grace_ms != new.timeout.start_grace_ms,
            "net.start_grace_ms",
        );
        check(self.admin_token_file != new.admin_token_file, "admin.token_file");
        check(self.status_enabled != new.status_enabled, "status.enabled");
        check(self.status_bind != new.status_bind, "status.bind");
        check(self.status_port != new.status_port, "status.port");
        check(self.log_dir != new.log_dir, "logs.dir");
        check(self.keep_log_files != new.keep_log_files, "logs.keep_files");
        check(self.persistence != new.persistence, "persistence");
        check(self.bots.min_players != new.bots.min_players, "bots.min_players");
        check(self.bots.max_bots != new.bots.max_bots, "bots.max_bots");
        check(
            self.bots.join_delay_ticks != new.bots.join_delay_ticks,
            "bots.join_delay_ticks",
        );
        check(
            self.bots.leave_delay_ticks != new.bots.leave_delay_ticks,
            "bots.leave_delay_ticks",
        );
        check(self.bots.seed != new.bots.seed, "bots.seed");
        check(
            self.bots.reaction_ticks != new.bots.reaction_ticks,
            "bots.reaction_ticks",
        );
        check(
            self.bots.aim_error_radians != new.bots.aim_error_radians,
            "bots.aim_error_radians",
        );
        check(
            self.bots.perception_radius != new.bots.perception_radius,
            "bots.perception_radius",
        );
        // A running match keeps its seats, delay and observer (ADR-8558).
        check(self.lockstep != new.lockstep, "lockstep");
        out
    }

    /// Hot-reloadable keys that differ (spec 76 § 5: name, motd, max_players, password, rotation,
    /// net rates).
    pub fn hot_changes(&self, new: &ServerSettings) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut check = |changed: bool, key: &'static str| {
            if changed {
                out.push(key);
            }
        };
        check(self.name != new.name, "server.name");
        check(self.motd != new.motd, "server.motd");
        check(self.max_players != new.max_players, "server.max_players");
        check(self.password != new.password, "server.password");
        check(self.rotation != new.rotation, "levels.rotation");
        check(self.playlist != new.playlist, "levels.playlist");
        check(
            self.client_rate_bytes != new.client_rate_bytes,
            "net.client_rate_bytes",
        );
        check(
            self.server_rate_bytes != new.server_rate_bytes,
            "net.server_rate_bytes",
        );
        check(
            self.send_every_ticks != new.send_every_ticks,
            "net.send_every_ticks",
        );
        // Negotiated per connection at the first ACK: a change applies to joins after the reload.
        check(self.entropy != new.entropy, "net.entropy");
        out
    }
}

/// Resolves the configuration: defaults < file < environment < command line, then validation.
/// `env` reads one environment variable (tests pass a map). A missing default `server.toml` is not
/// an error; a missing `--config` file is.
pub fn resolve_config(
    base_dir: &Path,
    cli: &CliOverrides,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedConfig, Vec<FieldError>> {
    let mut settings = ServerSettings::default();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let (path, required) = match &cli.config {
        Some(p) => (
            if p.is_absolute() {
                p.clone()
            } else {
                base_dir.join(p)
            },
            true,
        ),
        None => (base_dir.join("server.toml"), false),
    };
    let used = match std::fs::read_to_string(&path) {
        Ok(text) => {
            settings.apply_file(&text, &mut errors, &mut warnings);
            Some(path)
        }
        Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            errors.push(FieldError {
                field: "--config".into(),
                expected: format!("a readable file ({}: {e})", path.display()),
            });
            None
        }
    };
    settings.apply_overrides(env, cli, &mut errors);
    if errors.is_empty() {
        errors = settings.validate();
    }
    if errors.is_empty() {
        Ok(ResolvedConfig {
            settings,
            path: used,
            warnings,
        })
    } else {
        Err(errors)
    }
}

/// Resolves from the process environment.
pub fn resolve_from_process(base_dir: &Path, cli: &CliOverrides) -> Result<ResolvedConfig, Vec<FieldError>> {
    resolve_config(base_dir, cli, &|k| std::env::var(k).ok())
}

/// The commented default `server.toml` (written by `regia package --server`).
pub const DEFAULT_SERVER_TOML: &str = r#"format_version = 4

[server]
name = "My Game Server"           # shown in server lists and the welcome message
bind = "0.0.0.0"                  # address to listen on
port = 7777                       # UDP game port
max_players = 16                  # players, not peers (split-screen counts per player)
password = ""                     # empty = no password
tick_hz = 60                      # simulation rate
motd = ""                         # message of the day, sent with the welcome
idle_tick_hz = 10                 # tick rate while empty, if the game allows it

[levels]
rotation = ["levels/arena.level.ron"]  # played in order; loops
# playlist = "rotation.playlist.ron"  # exclusive with rotation: map, mode and options rows
start_index = 0
match_end_delay_s = 10

[net]
interrupt_ms = 1500              # report a connection problem without dropping the seat
timeout_ms = 20000               # silence-only removal
start_grace_ms = 30000           # admission floor before game traffic
mtu_probing = false               # probe the path for packets over 1200 bytes (1280/1400/1452)
# client_rate_bytes = 49152       # per client; absent = config/net.ron decides
# server_rate_bytes = 0           # 0 = no total cap; absent = config/net.ron decides
# send_every_ticks = 2            # absent = config/net.ron decides
# entropy = "off"                 # off | range; absent = config/net.ron decides (spec 72 § 18)
encryption = "auto"               # auto = every non-loopback peer; required = all; off = none
identity_file = "server.key"      # the server's key; clients pin its public half

[bots]
min_players = 0                   # desired humans + bots; 0 = no fill
max_bots = 0                      # 0 disables filler and reconnect-takeover bots
join_delay_ticks = 30             # human joins replace bots after this stable delay
leave_delay_ticks = 30            # missing filler bots return after this stable delay
seed = 0                          # deterministic patrol, perception and aim noise
reaction_ticks = 12
aim_error_radians = 0.03
perception_radius = 40.0

[admin]
token_file = "admin.token"        # remote admin token; missing file = remote admin off

[status]
enabled = false                   # HTTP health endpoint
bind = "127.0.0.1"
port = 7778

[logs]
dir = "logs"                      # relative to the server directory
keep_files = 10

[persistence]
enabled = false                   # keep the world across restarts (persistent co-op, survival)
dir = "saves"                     # relative to the server directory; one folder per level
autosave_ticks = 18000            # host ticks between autosaves (18000 = 5 min at 60 Hz); 0 = off
keep = 5                          # saves kept per level; a bad newest save falls back to the next
save_on_shutdown = true           # after the drain of a graceful shutdown
save_on_level_change = true       # the level being left, before a travel
player_identity = "platform"      # platform = authenticator id only; platform_or_name = else the name

[lockstep]
enabled = false                   # host a lockstep match (the game's lockstep type) instead of the game
max_seats = 40                    # seats the match takes at most (1..=4096)
input_delay_ticks = 4             # the input delay the match starts with
observer = true                   # step the simulation here too and vote in the hash quorum
checkpoint_ticks = 30             # ticks between hash votes; 0 = none
observer_max_lag_ticks = 60       # an observer further behind leaves the quorum until it catches up
observer_max_backlog_ticks = 600  # an observer further behind is detached for the match
"#;

/// Rewrites a `server.toml` to [`SERVER_TOML_FORMAT_VERSION`] (`regia upgrade-project`, ADR-3354).
/// v1 → v2 adds persistence; v2 → v3 bots/entropy; v3 → v4 timeouts and the optional
/// `levels.playlist` key (ADR-8345). All are optional, so the migrator rewrites the top-level
/// `format_version` line (or adds one) and keeps every other byte, comments included. The text
/// must parse as a supported source version first.
pub fn migrate_server_toml(text: &str) -> Result<String, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| e.message().to_owned())?;
    let from = match table.get("format_version") {
        None => 1,
        Some(v) => v
            .as_integer()
            .and_then(|i| u32::try_from(i).ok())
            .ok_or_else(|| "format_version is not a non-negative integer".to_owned())?,
    };
    if from == SERVER_TOML_FORMAT_VERSION {
        return Ok(text.to_owned());
    }
    if !(SERVER_TOML_MIN_FORMAT_VERSION..SERVER_TOML_FORMAT_VERSION).contains(&from) {
        return Err(format!("no migrator from server.toml v{from}"));
    }
    let mut out = String::with_capacity(text.len() + 24);
    let mut replaced = false;
    let mut in_table = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_table = true;
        }
        if !replaced && !in_table && trimmed.starts_with("format_version") {
            let rest = trimmed["format_version".len()..].trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                let value = value.trim_start();
                let digits = value.bytes().take_while(u8::is_ascii_digit).count();
                if digits > 0 {
                    let indent = &line[..line.len() - trimmed.len()];
                    out.push_str(indent);
                    out.push_str(&format!("format_version = {SERVER_TOML_FORMAT_VERSION}"));
                    out.push_str(&value[digits..]);
                    replaced = true;
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    if !replaced {
        out.insert_str(0, &format!("format_version = {SERVER_TOML_FORMAT_VERSION}\n"));
    }
    // The result must read as the current version with the same settings.
    let mut before = ServerSettings::default();
    let mut after = ServerSettings::default();
    let (mut e1, mut e2, mut w) = (Vec::new(), Vec::new(), Vec::new());
    before.apply_file(text, &mut e1, &mut w);
    after.apply_file(&out, &mut e2, &mut w);
    if !e2.is_empty() {
        return Err(e2.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "));
    }
    before.format_version = SERVER_TOML_FORMAT_VERSION;
    if before != after {
        return Err("the rewritten file reads differently".to_owned());
    }
    Ok(out)
}

/// What [`upgrade_config_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigUpgrade {
    /// The file was rewritten from this version to [`SERVER_TOML_FORMAT_VERSION`].
    Upgraded {
        /// The file that was rewritten.
        path: PathBuf,
        /// Its version before.
        from: u32,
    },
    /// The file already had the current version; nothing was written.
    Current {
        /// The file that was read.
        path: PathBuf,
    },
}

/// `--upgrade-config` (spec 76 § 4, ADR-5650): rewrites the host's config file (`--config PATH`,
/// else `server.toml` in `base_dir`) to [`SERVER_TOML_FORMAT_VERSION`] with
/// [`migrate_server_toml`], keeping comments; the write is atomic (a temporary file, then a
/// rename).
///
/// # Errors
///
/// A line naming the file when it cannot be read or written, or when the migrator refuses it
/// (an unknown version or a file that does not parse).
pub fn upgrade_config_file(base_dir: &Path, cli: &CliOverrides) -> Result<ConfigUpgrade, String> {
    let path = match &cli.config {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => base_dir.join(p),
        None => base_dir.join("server.toml"),
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let from = text
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| t.get("format_version").and_then(toml::Value::as_integer))
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1);
    let out = migrate_server_toml(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if out == text {
        return Ok(ConfigUpgrade::Current { path });
    }
    let tmp = path.with_extension("toml.upgrade-tmp");
    std::fs::write(&tmp, &out).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })?;
    Ok(ConfigUpgrade::Upgraded { path, from })
}

/// Whether `path` stays under the directory it is joined to.
fn inside_server_dir(path: &str) -> bool {
    use std::path::Component;
    let path = std::path::Path::new(path);
    !path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockstep::server::LockstepServerSettings;

    /// `[lockstep]` (optional, folded into the unreleased v4 by ADR-8558): off by default and in
    /// the template, every key written by `LockstepServerSettings::to_toml` reads back to the same
    /// settings, a restart-only section, and a v3 file without it loads with lockstep off.
    #[test]
    fn server_toml_lockstep_section_round_trips() {
        let default = ServerSettings::from_toml(DEFAULT_SERVER_TOML).expect("default template");
        assert_eq!(default.lockstep, LockstepServerSettings::default());
        assert!(!default.lockstep.enabled);
        assert_eq!(default.lockstep.max_seats, 40);
        let set = LockstepServerSettings {
            enabled: true,
            max_seats: 22,
            input_delay_ticks: 6,
            observer: false,
            checkpoint_ticks: 15,
            observer_max_lag_ticks: 45,
            observer_max_backlog_ticks: 900,
        };
        let text = format!(
            "format_version = 4\n[levels]\nrotation = [\"arena\"]\n\n{}",
            set.to_toml()
        );
        let mut warnings = Vec::new();
        let mut errors = Vec::new();
        let mut read = ServerSettings::default();
        read.apply_file(&text, &mut errors, &mut warnings);
        assert!(
            errors.is_empty() && warnings.is_empty(),
            "{errors:?} {warnings:?}\n{text}"
        );
        assert_eq!(read.lockstep, set);
        assert_eq!(ServerSettings::from_toml(&text).expect("valid").lockstep, set);
        // The core reader of the section agrees with the host's.
        let table: toml::Table = text.parse().expect("toml");
        assert_eq!(LockstepServerSettings::from_table(&table).expect("core").0, set);
        // A running match keeps its settings until a restart (the same file without the section
        // is the baseline, so only `[lockstep]` differs).
        let base = ServerSettings::from_toml("format_version = 4\n[levels]\nrotation = [\"arena\"]\n")
            .expect("base");
        assert_eq!(base.restart_only_changes(&read), vec!["lockstep"]);
        assert!(base.hot_changes(&read).is_empty());
        // v3 and older files have no section: lockstep stays off.
        let v3 =
            ServerSettings::from_toml("format_version = 3\n[levels]\nrotation = [\"arena\"]\n").expect("v3");
        assert!(!v3.lockstep.enabled);
        // An unknown key in the section warns and is ignored, like any other section's.
        let mut w = Vec::new();
        let mut e = Vec::new();
        ServerSettings::default().apply_file("[lockstep]\nseats = 3\n", &mut e, &mut w);
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(
            w,
            vec!["server.toml: unknown key `lockstep.seats` ignored".to_owned()]
        );
    }

    /// A wrong type or an out-of-range value in `[lockstep]` is reported by its field name.
    #[test]
    fn a_bad_lockstep_field_is_named() {
        let base = "format_version = 4\n[levels]\nrotation = [\"arena\"]\n[lockstep]\n";
        for (line, field) in [
            ("enabled = 1", "lockstep.enabled"),
            ("observer = \"yes\"", "lockstep.observer"),
            ("max_seats = 0", "lockstep.max_seats"),
            ("max_seats = 4097", "lockstep.max_seats"),
            ("max_seats = -3", "lockstep.max_seats"),
            ("input_delay_ticks = 0", "lockstep.input_delay_ticks"),
            ("checkpoint_ticks = 3601", "lockstep.checkpoint_ticks"),
            ("observer_max_lag_ticks = 0", "lockstep.observer_max_lag_ticks"),
            (
                "observer_max_lag_ticks = 100\nobserver_max_backlog_ticks = 99",
                "lockstep.observer_max_backlog_ticks",
            ),
        ] {
            let errors = ServerSettings::from_toml(&format!("{base}{line}\n")).expect_err(line);
            assert_eq!(
                errors.iter().map(|e| e.field.as_str()).collect::<Vec<_>>(),
                vec![field],
                "{line}: {errors:?}"
            );
            assert!(!errors[0].expected.is_empty());
        }
        let not_a_table = ServerSettings::from_toml("lockstep = 3\n[levels]\nrotation = [\"arena\"]\n")
            .expect_err("not a table");
        assert_eq!(not_a_table[0].field, "lockstep");
    }

    /// `[net] mtu_probing` (optional in v4, ADR-8300): off by default and in the default template,
    /// read as a boolean, anything else refused by name, and a restart-only key; an older file
    /// without it loads with probing off.
    #[test]
    fn mtu_probing_is_a_restart_only_boolean_off_by_default() {
        let default = ServerSettings::from_toml(DEFAULT_SERVER_TOML).expect("default template");
        assert!(!default.mtu_probing);
        assert_eq!(default.format_version, 4);
        let on = ServerSettings::from_toml(
            "format_version = 4\n[levels]\nrotation = [\"arena\"]\n[net]\nmtu_probing = true\n",
        )
        .expect("v4 with probing");
        assert!(on.mtu_probing);
        let err = ServerSettings::from_toml(
            "format_version = 4\n[levels]\nrotation = [\"arena\"]\n[net]\nmtu_probing = 1\n",
        )
        .expect_err("not a boolean");
        assert!(err.iter().any(|e| e.field == "net.mtu_probing"), "{err:?}");
        assert!(default.restart_only_changes(&on).contains(&"net.mtu_probing"));
        // A v3 file (no key) loads with probing off and migrates by its version line only.
        let v3 = "format_version = 3 # mine\n[levels]\nrotation = [\"arena\"]\n";
        assert!(!ServerSettings::from_toml(v3).expect("v3 loads").mtu_probing);
        let up = migrate_server_toml(v3).expect("v3 migrates");
        assert_eq!(
            up,
            "format_version = 4 # mine\n[levels]\nrotation = [\"arena\"]\n"
        );
    }

    #[test]
    fn default_file_parses_to_the_defaults_with_its_level() {
        let mut s = ServerSettings::default();
        let mut e = Vec::new();
        let mut w = Vec::new();
        s.apply_file(DEFAULT_SERVER_TOML, &mut e, &mut w);
        assert!(e.is_empty() && w.is_empty(), "{e:?} {w:?}");
        assert_eq!(s.name, "My Game Server");
        assert_eq!(s.rotation, vec!["levels/arena.level.ron".to_owned()]);
        assert!(s.validate().is_empty());
        // Review regression: texts that would not fit `Welcome` are refused at startup.
        let long = ServerSettings {
            name: "n".repeat(256),
            motd: "m".repeat(4097),
            rotation: vec!["l".repeat(256)],
            ..s
        };
        let fields: Vec<String> = long.validate().into_iter().map(|e| e.field).collect();
        assert_eq!(fields, ["server.name", "server.motd", "levels.rotation"]);
    }

    /// Audit L6 (muse): both paths stay inside the server directory (spec 76 § 4). Audit net1
    /// (platform): the server's key file is constrained the same way — `base_dir.join` would
    /// otherwise follow an absolute path or `..` and load or create the private key outside the
    /// server directory.
    #[test]
    fn absolute_and_escaping_paths_are_refused() {
        for path in ["/etc/passwd", "../up", "a/../../up", "/"] {
            let settings = ServerSettings {
                admin_token_file: path.into(),
                log_dir: path.into(),
                identity_file: path.into(),
                ..ServerSettings::default()
            };
            let fields: Vec<String> = settings.validate().into_iter().map(|e| e.field).collect();
            assert!(
                fields.iter().any(|f| f == "admin.token_file"),
                "{path}: {fields:?}"
            );
            assert!(fields.iter().any(|f| f == "logs.dir"), "{path}: {fields:?}");
            assert!(
                fields.iter().any(|f| f == "net.identity_file"),
                "{path}: {fields:?}"
            );
        }
        // An empty key path would join to the server directory itself; an empty token file is a
        // real setting (remote admin off) and stays allowed.
        let settings = ServerSettings {
            identity_file: String::new(),
            ..ServerSettings::default()
        };
        assert!(settings.validate().iter().any(|e| e.field == "net.identity_file"));
        let settings = ServerSettings {
            admin_token_file: "secrets/admin.token".into(),
            log_dir: "./logs/run1".into(),
            identity_file: "keys/server.key".into(),
            ..ServerSettings::default()
        };
        let fields: Vec<String> = settings.validate().into_iter().map(|e| e.field).collect();
        assert!(
            !fields
                .iter()
                .any(|f| f == "admin.token_file" || f == "logs.dir" || f == "net.identity_file"),
            "{fields:?}"
        );
    }

    /// Review row 13: out-of-range `[net]` silence values are refused at startup, one error per
    /// field, and the defaults and edges pass.
    #[test]
    fn timeout_policy_values_are_range_checked() {
        let fields = |interrupt_ms, timeout_ms, start_grace_ms| -> Vec<String> {
            ServerSettings {
                timeout: crate::session::TimeoutPolicy {
                    interrupt_ms,
                    timeout_ms,
                    start_grace_ms,
                },
                ..ServerSettings::default()
            }
            .validate()
            .into_iter()
            .map(|e| e.field)
            // The bare defaults have no level rotation; only the silence keys matter here.
            .filter(|f| f.ends_with("_ms"))
            .collect()
        };
        // An author thinking in seconds.
        assert_eq!(fields(1_500, 20, 30_000), ["net.timeout_ms", "net.interrupt_ms"]);
        assert_eq!(fields(1_500, 999, 30_000), ["net.timeout_ms", "net.interrupt_ms"]);
        // An interruption every tick, or never before the timeout.
        assert_eq!(fields(0, 20_000, 30_000), ["net.interrupt_ms"]);
        assert_eq!(fields(99, 20_000, 30_000), ["net.interrupt_ms"]);
        assert_eq!(fields(20_000, 20_000, 30_000), ["net.interrupt_ms"]);
        assert_eq!(fields(25_000, 20_000, 30_000), ["net.interrupt_ms"]);
        // A grace past ten minutes.
        assert_eq!(fields(1_500, 20_000, 600_001), ["net.start_grace_ms"]);
        // The defaults and the edges are valid.
        assert!(fields(1_500, 20_000, 30_000).is_empty());
        assert!(fields(100, 1_000, 0).is_empty());
        assert!(fields(999, 1_000, 600_000).is_empty());
        let text = "format_version = 4\n[levels]\nrotation = [\"arena\"]\n[net]\ntimeout_ms = 20\n";
        let err = ServerSettings::from_toml(text).expect_err("refused");
        assert!(err.iter().any(|e| e.field == "net.timeout_ms"), "{err:?}");
    }

    /// Windows drive-prefixed paths replace the base directory at `Path::join`, so they escape
    /// the server directory even when they are not absolute (audit net1, platform).
    #[cfg(windows)]
    #[test]
    fn windows_drive_prefixed_paths_are_refused() {
        for path in [
            "C:server.key",
            "C:/Users/Public/server.key",
            "D:\\keys\\server.key",
        ] {
            let settings = ServerSettings {
                identity_file: path.into(),
                ..ServerSettings::default()
            };
            let fields: Vec<String> = settings.validate().into_iter().map(|e| e.field).collect();
            assert!(
                fields.iter().any(|f| f == "net.identity_file"),
                "{path}: {fields:?}"
            );
        }
    }

    #[test]
    fn bot_config_is_additive_and_the_cli_sets_fill_and_cap() {
        let settings = ServerSettings::from_toml(
            r#"
format_version = 1
levels.rotation = ["arena"]
[bots]
min_players = 4
max_bots = 6
seed = 91
reaction_ticks = 3
aim_error_radians = 0.2
perception_radius = 25.0
"#,
        )
        .expect("valid bot config");
        assert_eq!(settings.bots.min_players, 4);
        assert_eq!(settings.bots.max_bots, 6);
        assert_eq!(settings.bots.seed, 91);
        assert_eq!(settings.bots.join_delay_ticks, 30);
        let cli = CliOverrides::parse(&["--bots".into(), "3".into()]).expect("cli");
        let resolved = resolve_config(Path::new("/definitely/missing"), &cli, &|key| {
            (key == "REGIA_SERVER_LEVEL").then(|| "arena".into())
        })
        .expect("resolved");
        assert_eq!(resolved.settings.bots.min_players, 3);
        assert_eq!(resolved.settings.bots.max_bots, 3);
        let mut restarted = settings.clone();
        restarted.bots.max_bots = 9;
        assert!(
            settings
                .restart_only_changes(&restarted)
                .contains(&"bots.max_bots")
        );
        assert!(!settings.hot_changes(&restarted).contains(&"bots.max_bots"));
    }

    #[test]
    fn cli_parse() {
        let args: Vec<String> = [
            "--port",
            "9000",
            "--level=levels/b.level.ron",
            "--console",
            "--bots=4",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let cli = CliOverrides::parse(&args).expect("parse");
        assert_eq!(cli.port.as_deref(), Some("9000"));
        assert_eq!(cli.level.as_deref(), Some("levels/b.level.ron"));
        assert!(cli.console);
        assert_eq!(cli.bots.as_deref(), Some("4"));
        assert!(CliOverrides::parse(&["--nope".to_owned()]).is_err());
        assert!(CliOverrides::parse(&["--port".to_owned()]).is_err());
    }

    #[test]
    fn rotation_and_playlist_together_is_a_config_error() {
        let dir = regia_test_utils::unique_temp_dir("rotation-and-playlist");
        let text = r#"
format_version = 4
[server]
name = "Both"
[levels]
rotation = ["levels/a.level.ron"]
playlist = "rows.playlist.ron"
"#;
        regia_test_utils::write_atomic(&dir.join("server.toml"), text.as_bytes()).expect("server.toml");
        let errors = resolve_config(&dir, &CliOverrides::default(), &|_| None).expect_err("both keys");
        assert!(
            errors
                .iter()
                .any(|error| { error.field == "levels.playlist" && error.expected.contains("exclusive") }),
            "{errors:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
