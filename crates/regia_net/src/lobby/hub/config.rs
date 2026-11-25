// SPDX-License-Identifier: MPL-2.0
//! `hub.toml` for the `regia-lobby-hub` process (NL-06, ADR-8648; v2 is NL-11, ADR-10397).
//!
//! The file is read key by key. A bad value or an unknown key is a named field error
//! (`field: expected ...`), the same shape as `server.toml`. The server key path is created on
//! first run and is refused when it sits inside a project (a directory whose ancestor holds
//! `Regia.toml`). An empty `key_path` means the user config directory, never a path next to the
//! package.
//!
//! Version 2 adds `[relay]`, `[bans]` and `[log]`. A version-1 file still loads: with no
//! `[relay]` table the relay stays off, which is what v0.7.0 shipped. [`migrate_hub_toml`]
//! rewrites that file to version 2 and appends `relay.enabled = false`.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use regia_core::schema::join_field_lists;

use super::HubSettings;
use crate::crypto::PlayerKey;

/// The `format_version` this build writes. It still reads version 1 (ADR-10397).
pub const HUB_TOML_FORMAT_VERSION: u32 = 2;

/// Default UDP listen port (the DO27 hub listen default, kept as a setting).
pub const DEFAULT_HUB_PORT: u16 = 7810;

/// Default health-endpoint port, bound to loopback.
pub const DEFAULT_STATUS_PORT: u16 = 7811;

/// Default relay UDP port. Not the hub port (NL-11).
pub const DEFAULT_RELAY_PORT: u16 = 7812;

/// Allocation ports a fresh `hub.toml` asks the relay to open.
pub const DEFAULT_RELAY_ALLOCATION_CAP: u16 = 256;

/// Relay control requests one source address may make per second, unless `hub.toml` says otherwise.
pub const DEFAULT_RELAY_PER_ADDRESS_PER_SEC: u32 = 30;

/// Player keys one ban file may list. The same cap as `HubServer::set_operator_bans`.
const BAN_FILE_CAP: usize = 4_096;

/// Requests one address may make per second, unless `hub.toml` says otherwise.
pub const DEFAULT_PER_ADDRESS_PER_SEC: u32 = 30;

/// Requests one player key may make per second, unless `hub.toml` says otherwise.
pub const DEFAULT_PER_IDENTITY_PER_SEC: u32 = 20;

/// Every key `hub.toml` v2 reads, in the order of `docs/formats/hub-toml.md`.
pub const HUB_TOML_KEYS: &[&str] = &[
    "format_version",
    "bind",
    "port",
    "key_path",
    "caps.session_cap",
    "caps.lobby_cap",
    "caps.max_members",
    "caps.chat_per_sec",
    "caps.handle_max_bytes",
    "caps.idle_ms",
    "limits.per_address_per_sec",
    "limits.per_identity_per_sec",
    "limits.ask_per_identity_per_10s",
    "limits.ask_per_address_per_10s",
    "limits.ask_inflight_per_identity",
    "limits.ask_inflight_hub",
    "status.enabled",
    "status.bind",
    "status.port",
    "relay.enabled",
    "relay.bind",
    "relay.port",
    "relay.allocation_cap",
    "relay.per_address_per_sec",
    "bans.path",
    "log.dir",
];

/// The schema fingerprint `cargo xtask check-formats` hashes (one field list).
#[must_use]
pub fn hub_toml_schema_fingerprint() -> String {
    join_field_lists(&[HUB_TOML_KEYS.iter().map(|k| (*k).to_owned()).collect()])
}

/// Versions this file migrates from. v1 gains `[relay]`, `[bans]` and `[log]` (ADR-10397).
#[must_use]
pub fn hub_toml_migration_sources() -> Vec<u32> {
    vec![1]
}

/// The `hub.toml` a package stages and a process with no file uses. The relay is on.
pub const DEFAULT_HUB_TOML: &str = "\
# Lobby hub (hub.toml v2). The key file is created on first run outside any project.
format_version = 2
bind = \"0.0.0.0\"
port = 7810
key_path = \"\"

[caps]
session_cap = 1024
lobby_cap = 256
max_members = 64
chat_per_sec = 4
handle_max_bytes = 24
idle_ms = 30000

[limits]
per_address_per_sec = 30
per_identity_per_sec = 20

[status]
enabled = true
bind = \"127.0.0.1\"
port = 7811

[relay]
enabled = true
bind = \"0.0.0.0\"
port = 7812
allocation_cap = 256
per_address_per_sec = 30

[bans]
path = \"hub-bans.txt\"

[log]
dir = \"logs\"
";

/// One field-level configuration error (`field: expected ...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubFieldError {
    /// `section.key` or the file name.
    pub field: String,
    /// What was expected.
    pub expected: String,
}

impl std::fmt::Display for HubFieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: expected {}", self.field, self.expected)
    }
}

/// Caps `hub.toml` may set. Unset keys keep [`HubSettings::default`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubCaps {
    /// Most sessions.
    pub session_cap: usize,
    /// Most lobbies.
    pub lobby_cap: usize,
    /// Largest member limit a lobby may ask for.
    pub max_members: u32,
    /// Chat lines one session may send per second, per scope.
    pub chat_per_sec: usize,
    /// Longest handle in bytes.
    pub handle_max_bytes: usize,
    /// Silence after which a session expires, in milliseconds.
    pub idle_ms: u64,
}

impl Default for HubCaps {
    fn default() -> Self {
        let settings = HubSettings::default();
        Self {
            session_cap: settings.session_cap,
            lobby_cap: settings.lobby_cap,
            max_members: settings.max_members,
            chat_per_sec: settings.chat_per_sec,
            handle_max_bytes: settings.handle_max_bytes,
            idle_ms: settings.idle_ms,
        }
    }
}

/// Rate limits that sit in front of the hub core, including its member-data limits, plus the
/// AskReach caps the core reads (spec 89 § 16.5). The four ask keys are optional: a v1 file that
/// omits them keeps the wire caps. [`HubFile::settings`] clamps each ask cap at or below the wire
/// cap and never raises one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubLimits {
    /// Requests per second from one IP address.
    pub per_address_per_sec: u32,
    /// Requests per second from one player key.
    pub per_identity_per_sec: u32,
    /// `AskReach` calls one player key may make per 10 seconds. Wire cap 4.
    pub ask_per_identity_per_10s: u32,
    /// `AskReach` calls one source address may make per 10 seconds. Wire cap 16.
    pub ask_per_address_per_10s: u32,
    /// Open asks one player key may hold. Wire cap 2.
    pub ask_inflight_per_identity: u32,
    /// Open asks the hub may hold. Wire cap 64.
    pub ask_inflight_hub: u32,
}

impl Default for HubLimits {
    fn default() -> Self {
        Self {
            per_address_per_sec: DEFAULT_PER_ADDRESS_PER_SEC,
            per_identity_per_sec: DEFAULT_PER_IDENTITY_PER_SEC,
            ask_per_identity_per_10s: super::limits::ASK_PER_IDENTITY_CAP,
            ask_per_address_per_10s: super::limits::ASK_PER_ADDRESS_CAP,
            ask_inflight_per_identity: super::limits::ASK_INFLIGHT_PER_IDENTITY_CAP,
            ask_inflight_hub: super::limits::ASK_INFLIGHT_HUB_CAP,
        }
    }
}

/// Whether `GET /health` is served, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubStatusFile {
    /// `status.enabled`.
    pub enabled: bool,
    /// `status.bind`.
    pub bind: String,
    /// `status.port`.
    pub port: u16,
}

impl Default for HubStatusFile {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1".into(),
            port: DEFAULT_STATUS_PORT,
        }
    }
}

/// `[relay]` in `hub.toml` (NL-11, ADR-10397). A fresh file enables it. A migrated v1 file does not.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HubRelayFile {
    /// `relay.enabled`. When false the process does not bind the relay port.
    pub enabled: bool,
    /// `relay.bind`.
    pub bind: String,
    /// `relay.port`. Not the hub listen port.
    pub port: u16,
    /// Allocation sockets the relay opens.
    pub allocation_cap: u16,
    /// Relay control requests one source address may make per second.
    pub per_address_per_sec: u32,
}

impl Default for HubRelayFile {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "0.0.0.0".into(),
            port: DEFAULT_RELAY_PORT,
            allocation_cap: DEFAULT_RELAY_ALLOCATION_CAP,
            per_address_per_sec: DEFAULT_RELAY_PER_ADDRESS_PER_SEC,
        }
    }
}

/// `[bans]` in `hub.toml`. One player key per line. Addresses are not stored here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HubBansFile {
    /// `bans.path`. A missing file is an empty set.
    pub path: String,
}

impl Default for HubBansFile {
    fn default() -> Self {
        Self {
            path: "hub-bans.txt".into(),
        }
    }
}

/// `[log]` in `hub.toml`. The process creates the directory and appends `hub.log`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HubLogFile {
    /// `log.dir`.
    pub dir: String,
}

impl Default for HubLogFile {
    fn default() -> Self {
        Self { dir: "logs".into() }
    }
}

/// A parsed `hub.toml`, including the v2 tables [`HubFile`] does not store.
///
/// [`HubFile`] keeps the v1 field list. The process reads this document.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct HubDocument {
    /// Listen address, caps, limits and the health endpoint.
    pub file: HubFile,
    /// The relay.
    pub relay: HubRelayFile,
    /// The operator ban file.
    pub bans: HubBansFile,
    /// The log directory.
    pub log: HubLogFile,
}

impl HubDocument {
    /// Parses `hub.toml` text over the built-in defaults.
    ///
    /// Version 1 and version 2 both load. A missing `format_version` is version 1. A version-1
    /// file that omits `relay.enabled` leaves the relay off. A version-2 file that omits it
    /// leaves the relay on.
    ///
    /// # Errors
    ///
    /// One [`HubFieldError`] per bad or unknown field. The display is `field: expected ...`.
    pub fn from_toml(text: &str) -> Result<Self, Vec<HubFieldError>> {
        if text.trim().is_empty() {
            return Err(vec![HubFieldError {
                field: "hub.toml".into(),
                expected: "a hub.toml document".into(),
            }]);
        }
        let mut file = HubFile::default();
        let mut relay = HubRelayFile::default();
        let mut bans = HubBansFile::default();
        let mut log = HubLogFile::default();
        let mut errors = Vec::new();
        let mut saw_version = false;
        let mut saw_relay_enabled = false;
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(err) => {
                errors.push(HubFieldError {
                    field: "hub.toml".into(),
                    expected: format!("valid TOML ({})", err.message()),
                });
                return Err(errors);
            }
        };
        for (key, value) in &table {
            match key.as_str() {
                "format_version" => {
                    saw_version = true;
                    if let Some(n) = expect_u64("format_version", value, &mut errors) {
                        if n == 1 || n == u64::from(HUB_TOML_FORMAT_VERSION) {
                            file.format_version = u32::try_from(n).unwrap_or(HUB_TOML_FORMAT_VERSION);
                        } else {
                            bad(
                                &mut errors,
                                "format_version",
                                &format!("1 or {HUB_TOML_FORMAT_VERSION}, found {n}"),
                            );
                        }
                    }
                }
                "bind" => {
                    if let Some(text) = expect_str("bind", value, &mut errors) {
                        file.bind = text;
                    }
                }
                "port" => {
                    if let Some(n) = expect_u64("port", value, &mut errors) {
                        file.port = fit_u16("port", n, &mut errors).unwrap_or(file.port);
                    }
                }
                "key_path" => {
                    if let Some(text) = expect_str("key_path", value, &mut errors) {
                        file.key_path = text;
                    }
                }
                "caps" => apply_caps(value, &mut file.caps, &mut errors),
                "limits" => apply_limits(value, &mut file.limits, &mut errors),
                "status" => apply_status(value, &mut file.status, &mut errors),
                "relay" => apply_relay(value, &mut relay, &mut saw_relay_enabled, &mut errors),
                "bans" => apply_bans(value, &mut bans, &mut errors),
                "log" => apply_log(value, &mut log, &mut errors),
                other => bad(
                    &mut errors,
                    other,
                    "one of format_version, bind, port, key_path, caps, limits, status, relay, bans, log",
                ),
            }
        }
        if !saw_version {
            file.format_version = 1;
        }
        if !saw_relay_enabled {
            relay.enabled = file.format_version == HUB_TOML_FORMAT_VERSION;
        }
        if errors.is_empty() {
            file.check_ranges(&mut errors);
            check_relay(&relay, &mut errors);
            check_path("bans.path", &bans.path, &mut errors);
            check_path("log.dir", &log.dir, &mut errors);
        }
        if errors.is_empty() {
            Ok(Self {
                file,
                relay,
                bans,
                log,
            })
        } else {
            Err(errors)
        }
    }

    /// The relay's UDP address when `relay.enabled` is set.
    ///
    /// # Errors
    ///
    /// `relay.bind` is not an IP address, or `relay.port` is 0.
    pub fn relay_addr(&self) -> Result<Option<SocketAddr>, HubFieldError> {
        if !self.relay.enabled {
            return Ok(None);
        }
        let ip = self.relay.bind.parse::<IpAddr>().map_err(|_| HubFieldError {
            field: "relay.bind".into(),
            expected: "an IP address such as \"0.0.0.0\"".into(),
        })?;
        if self.relay.port == 0 {
            return Err(HubFieldError {
                field: "relay.port".into(),
                expected: "an integer 1..=65535".into(),
            });
        }
        Ok(Some(SocketAddr::new(ip, self.relay.port)))
    }
}

/// Rewrites a version-1 `hub.toml` to [`HUB_TOML_FORMAT_VERSION`].
///
/// The migrator keeps every existing byte except the top-level `format_version` line, then
/// appends any of `[relay]`, `[bans]` and `[log]` that the file does not already have. The
/// appended relay has `enabled = false`, so an existing hub does not open port 7812 until an
/// operator turns it on. A file that is already version 2 is returned unchanged. AskReach keys
/// under `[limits]` stay optional and are not inserted.
///
/// # Errors
///
/// The text is not TOML, the version is not 1 or 2, or the rewritten file does not load.
pub fn migrate_hub_toml(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("hub.toml: expected a hub.toml document".to_owned());
    }
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
    if from == HUB_TOML_FORMAT_VERSION {
        return Ok(text.to_owned());
    }
    if from != 1 {
        return Err(format!("no migrator from hub.toml v{from}"));
    }
    // A version-1 `[relay]` table without `enabled` loads with the relay off. Version 2 defaults
    // a missing `enabled` to on, so the migrator writes `enabled = false` into that table.
    let relay_table = table.get("relay").and_then(toml::Value::as_table);
    let relay_enabled_before = relay_table
        .and_then(|relay| relay.get("enabled"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let needs_relay_off = relay_table.is_some_and(|relay| !relay.contains_key("enabled"));
    let mut wrote_relay_off = false;
    let mut out = String::with_capacity(text.len() + 256);
    let mut replaced = false;
    let mut in_table = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_table = true;
            if needs_relay_off && !wrote_relay_off && is_relay_header(trimmed) {
                out.push_str(line);
                if !line.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("enabled = false\n");
                wrote_relay_off = true;
                continue;
            }
        }
        if !replaced && !in_table && trimmed.starts_with("format_version") {
            let rest = trimmed["format_version".len()..].trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                let value = value.trim_start();
                // The whole integer token: TOML spells 1 as `1`, `+1`, `0x1`, `0o1` or `0b1`.
                let token = value
                    .bytes()
                    .take_while(|b| !b.is_ascii_whitespace() && *b != b'#')
                    .count();
                if token > 0 {
                    let indent = &line[..line.len() - trimmed.len()];
                    out.push_str(indent);
                    out.push_str(&format!("format_version = {HUB_TOML_FORMAT_VERSION}"));
                    out.push_str(&value[token..]);
                    replaced = true;
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    if !replaced {
        out.insert_str(0, &format!("format_version = {HUB_TOML_FORMAT_VERSION}\n"));
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !table.contains_key("relay") {
        out.push_str(
            "\n[relay]\nenabled = false\nbind = \"0.0.0.0\"\nport = 7812\nallocation_cap = 256\nper_address_per_sec = 30\n",
        );
    }
    if !table.contains_key("bans") {
        out.push_str("\n[bans]\npath = \"hub-bans.txt\"\n");
    }
    if !table.contains_key("log") {
        out.push_str("\n[log]\ndir = \"logs\"\n");
    }
    let doc = HubDocument::from_toml(&out).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    if needs_relay_off && !wrote_relay_off {
        // An inline or dotted `relay` table: write the key by hand rather than guess.
        return Err(
            "hub.toml v1 has a relay table with no `enabled` key that is not a `[relay]` section; \
             add `enabled = false` to it and run the upgrade again"
                .to_owned(),
        );
    }
    if doc.relay.enabled != relay_enabled_before {
        return Err("migrated hub.toml changed whether the relay is enabled".to_owned());
    }
    Ok(out)
}

/// True for a `[relay]` table header line, with optional spaces and a trailing comment.
fn is_relay_header(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix('[') else {
        return false;
    };
    let Some((name, after)) = rest.split_once(']') else {
        return false;
    };
    let after = after.trim_start();
    name.trim() == "relay" && (after.is_empty() || after.starts_with('#'))
}

/// Reads operator bans from `path`. A missing file is an empty list. A line that is blank or
/// starts with `#` is skipped. Anything else must be one player key, 64 hex digits.
///
/// # Errors
///
/// The file cannot be read, a line is not a player key, or the file lists more than 4,096 keys.
pub fn load_ban_keys(path: &Path) -> Result<Vec<PlayerKey>, HubFieldError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).map_err(|e| HubFieldError {
        field: "bans.path".into(),
        expected: format!("a readable file ({e})"),
    })?;
    let mut keys = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(key) = PlayerKey::parse(line) else {
            return Err(HubFieldError {
                field: "bans.path".into(),
                expected: format!("a player key (64 hex digits) on line {}, found {line}", i + 1),
            });
        };
        keys.push(key);
        if keys.len() > BAN_FILE_CAP {
            return Err(HubFieldError {
                field: "bans.path".into(),
                expected: format!("at most {BAN_FILE_CAP} player keys"),
            });
        }
    }
    Ok(keys)
}

/// A parsed `hub.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubFile {
    /// `format_version`.
    pub format_version: u32,
    /// UDP bind address.
    pub bind: String,
    /// UDP port.
    pub port: u16,
    /// Server key path. Empty means [`default_key_path`].
    pub key_path: String,
    /// Directory and session caps.
    pub caps: HubCaps,
    /// Per-address and per-identity request limits.
    pub limits: HubLimits,
    /// Health endpoint.
    pub status: HubStatusFile,
}

impl Default for HubFile {
    fn default() -> Self {
        Self {
            format_version: HUB_TOML_FORMAT_VERSION,
            bind: "0.0.0.0".into(),
            port: DEFAULT_HUB_PORT,
            key_path: String::new(),
            caps: HubCaps::default(),
            limits: HubLimits::default(),
            status: HubStatusFile::default(),
        }
    }
}

impl HubFile {
    /// Parses `hub.toml` text over the built-in defaults.
    ///
    /// The v2 tables are checked and then dropped: this type keeps the listen, cap, limit and
    /// status fields. Use [`HubDocument::from_toml`] when the process needs the relay, the ban
    /// file or the log directory.
    ///
    /// # Errors
    ///
    /// One [`HubFieldError`] per bad or unknown field. The display is `field: expected ...`.
    pub fn from_toml(text: &str) -> Result<Self, Vec<HubFieldError>> {
        // A missing file uses the built-in document. A blank file is not that document: the fuzz
        // harness rejects empty text, and a cut through a quoted value must fail the parse.
        HubDocument::from_toml(text).map(|doc| doc.file)
    }

    /// The hub caps as [`HubSettings`], with every field this file does not set left at the
    /// DO27 default.
    #[must_use]
    pub fn settings(&self) -> HubSettings {
        HubSettings {
            session_cap: self.caps.session_cap,
            lobby_cap: self.caps.lobby_cap,
            max_members: self.caps.max_members,
            chat_per_sec: self.caps.chat_per_sec,
            handle_max_bytes: self.caps.handle_max_bytes,
            idle_ms: self.caps.idle_ms,
            ask_per_identity_per_10s: self.limits.ask_per_identity_per_10s,
            ask_per_address_per_10s: self.limits.ask_per_address_per_10s,
            ask_inflight_per_identity: self.limits.ask_inflight_per_identity,
            ask_inflight_hub: self.limits.ask_inflight_hub,
            ..HubSettings::default()
        }
        .sanitized()
    }

    /// The UDP address the process binds.
    ///
    /// # Errors
    ///
    /// `bind` is not an IP address, or `port` is 0.
    pub fn listen_addr(&self) -> Result<SocketAddr, HubFieldError> {
        let ip = self.bind.parse::<IpAddr>().map_err(|_| HubFieldError {
            field: "bind".into(),
            expected: "an IP address such as \"0.0.0.0\"".into(),
        })?;
        if self.port == 0 {
            return Err(HubFieldError {
                field: "port".into(),
                expected: "an integer 1..=65535".into(),
            });
        }
        Ok(SocketAddr::new(ip, self.port))
    }

    /// The health endpoint address, when `status.enabled` is set.
    ///
    /// # Errors
    ///
    /// `status.bind` is not an IP address, or `status.port` is 0.
    pub fn status_addr(&self) -> Result<Option<SocketAddr>, HubFieldError> {
        if !self.status.enabled {
            return Ok(None);
        }
        let ip = self.status.bind.parse::<IpAddr>().map_err(|_| HubFieldError {
            field: "status.bind".into(),
            expected: "an IP address such as \"127.0.0.1\"".into(),
        })?;
        if self.status.port == 0 {
            return Err(HubFieldError {
                field: "status.port".into(),
                expected: "an integer 1..=65535".into(),
            });
        }
        Ok(Some(SocketAddr::new(ip, self.status.port)))
    }

    /// The key path: `key_path` when it is set, otherwise [`default_key_path`].
    #[must_use]
    pub fn resolved_key_path(&self) -> PathBuf {
        if self.key_path.is_empty() {
            default_key_path()
        } else {
            PathBuf::from(&self.key_path)
        }
    }

    fn check_ranges(&self, errors: &mut Vec<HubFieldError>) {
        if self.bind.parse::<IpAddr>().is_err() {
            bad(errors, "bind", "an IP address such as \"0.0.0.0\"");
        }
        if self.port == 0 {
            bad(errors, "port", "an integer 1..=65535");
        }
        if self.caps.session_cap == 0 || self.caps.session_cap > (1 << 20) {
            bad(errors, "caps.session_cap", "an integer 1..=1048576");
        }
        if self.caps.lobby_cap == 0 || self.caps.lobby_cap > (1 << 16) {
            bad(errors, "caps.lobby_cap", "an integer 1..=65536");
        }
        if self.caps.max_members == 0 || self.caps.max_members > crate::lobby::MAX_LOBBY_MEMBERS {
            bad(errors, "caps.max_members", "an integer 1..=250");
        }
        if self.caps.chat_per_sec == 0 {
            bad(errors, "caps.chat_per_sec", "an integer 1 or more");
        }
        if self.caps.handle_max_bytes == 0 || self.caps.handle_max_bytes > super::HANDLE_LIMIT_BYTES {
            bad(errors, "caps.handle_max_bytes", "an integer 1..=32");
        }
        if self.caps.idle_ms == 0 {
            bad(errors, "caps.idle_ms", "an integer 1 or more");
        }
        if self.limits.per_address_per_sec == 0 {
            bad(errors, "limits.per_address_per_sec", "an integer 1 or more");
        }
        if self.limits.per_identity_per_sec == 0 {
            bad(errors, "limits.per_identity_per_sec", "an integer 1 or more");
        }
        if self.limits.ask_per_identity_per_10s == 0 {
            bad(errors, "limits.ask_per_identity_per_10s", "an integer 1 or more");
        }
        if self.limits.ask_per_address_per_10s == 0 {
            bad(errors, "limits.ask_per_address_per_10s", "an integer 1 or more");
        }
        if self.limits.ask_inflight_per_identity == 0 {
            bad(errors, "limits.ask_inflight_per_identity", "an integer 1 or more");
        }
        if self.limits.ask_inflight_hub == 0 {
            bad(errors, "limits.ask_inflight_hub", "an integer 1 or more");
        }
        if self.status.enabled && self.status.bind.parse::<IpAddr>().is_err() {
            bad(errors, "status.bind", "an IP address such as \"127.0.0.1\"");
        }
        if self.status.enabled && self.status.port == 0 {
            bad(errors, "status.port", "an integer 1..=65535");
        }
    }
}

/// Where a missing `key_path` is created: `$XDG_CONFIG_HOME/regia/lobby-hub.key`, else
/// `$HOME/.config/regia/lobby-hub.key`, else `%APPDATA%\\regia\\lobby-hub.key`.
#[must_use]
pub fn default_key_path() -> PathBuf {
    if let Some(root) = env_dir("XDG_CONFIG_HOME") {
        return root.join("regia").join("lobby-hub.key");
    }
    if let Some(home) = env_dir("HOME") {
        return home.join(".config").join("regia").join("lobby-hub.key");
    }
    if let Some(appdata) = env_dir("APPDATA") {
        return appdata.join("regia").join("lobby-hub.key");
    }
    PathBuf::from("lobby-hub.key")
}

/// True when `path` lies in a project: the path or an ancestor directory holds `Regia.toml`.
#[must_use]
pub fn key_path_is_inside_project(path: &Path) -> bool {
    let mut dir = if path.is_dir() { Some(path) } else { path.parent() };
    while let Some(current) = dir {
        if current.join("Regia.toml").is_file() {
            return true;
        }
        dir = current.parent();
    }
    false
}

/// Refuses a key path that sits inside a project.
///
/// # Errors
///
/// `key_path` is inside a directory that holds `Regia.toml`.
pub fn validate_key_path(path: &Path) -> Result<(), HubFieldError> {
    if key_path_is_inside_project(path) {
        Err(HubFieldError {
            field: "key_path".into(),
            expected: "a path outside any project (no ancestor Regia.toml); the key is created on first run and is never placed inside a project".into(),
        })
    } else {
        Ok(())
    }
}

fn env_dir(name: &str) -> Option<PathBuf> {
    let value = std::env::var(name).ok()?;
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn bad(errors: &mut Vec<HubFieldError>, field: &str, expected: &str) {
    errors.push(HubFieldError {
        field: field.to_owned(),
        expected: expected.to_owned(),
    });
}

fn type_name(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

fn expect_str(field: &str, value: &toml::Value, errors: &mut Vec<HubFieldError>) -> Option<String> {
    match value.as_str() {
        Some(text) => Some(text.to_owned()),
        None => {
            bad(errors, field, &format!("a string, found {}", type_name(value)));
            None
        }
    }
}

fn expect_u64(field: &str, value: &toml::Value, errors: &mut Vec<HubFieldError>) -> Option<u64> {
    match value.as_integer() {
        Some(n) if n >= 0 => u64::try_from(n).ok(),
        Some(_) => {
            bad(errors, field, "a non-negative integer");
            None
        }
        None => {
            bad(errors, field, &format!("an integer, found {}", type_name(value)));
            None
        }
    }
}

fn expect_bool(field: &str, value: &toml::Value, errors: &mut Vec<HubFieldError>) -> Option<bool> {
    match value.as_bool() {
        Some(flag) => Some(flag),
        None => {
            bad(
                errors,
                field,
                &format!("true or false, found {}", type_name(value)),
            );
            None
        }
    }
}

fn expect_table<'a>(
    field: &str,
    value: &'a toml::Value,
    errors: &mut Vec<HubFieldError>,
) -> Option<&'a toml::Table> {
    match value.as_table() {
        Some(table) => Some(table),
        None => {
            bad(errors, field, &format!("a table, found {}", type_name(value)));
            None
        }
    }
}

fn fit_u16(field: &str, n: u64, errors: &mut Vec<HubFieldError>) -> Option<u16> {
    u16::try_from(n).ok().filter(|port| *port > 0).or_else(|| {
        bad(errors, field, "an integer 1..=65535");
        None
    })
}

fn apply_caps(value: &toml::Value, caps: &mut HubCaps, errors: &mut Vec<HubFieldError>) {
    let Some(table) = expect_table("caps", value, errors) else {
        return;
    };
    for (key, value) in table {
        let field = format!("caps.{key}");
        match key.as_str() {
            "session_cap" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    caps.session_cap = usize::try_from(n).unwrap_or(usize::MAX);
                }
            }
            "lobby_cap" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    caps.lobby_cap = usize::try_from(n).unwrap_or(usize::MAX);
                }
            }
            "max_members" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    caps.max_members = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
            "chat_per_sec" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    caps.chat_per_sec = usize::try_from(n).unwrap_or(usize::MAX);
                }
            }
            "handle_max_bytes" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    caps.handle_max_bytes = usize::try_from(n).unwrap_or(usize::MAX);
                }
            }
            "idle_ms" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    caps.idle_ms = n;
                }
            }
            _ => bad(
                errors,
                &field,
                "one of session_cap, lobby_cap, max_members, chat_per_sec, handle_max_bytes, idle_ms",
            ),
        }
    }
}

fn apply_limits(value: &toml::Value, limits: &mut HubLimits, errors: &mut Vec<HubFieldError>) {
    let Some(table) = expect_table("limits", value, errors) else {
        return;
    };
    for (key, value) in table {
        let field = format!("limits.{key}");
        match key.as_str() {
            "per_address_per_sec" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    limits.per_address_per_sec = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
            "per_identity_per_sec" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    limits.per_identity_per_sec = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
            "ask_per_identity_per_10s" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    limits.ask_per_identity_per_10s = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
            "ask_per_address_per_10s" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    limits.ask_per_address_per_10s = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
            "ask_inflight_per_identity" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    limits.ask_inflight_per_identity = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
            "ask_inflight_hub" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    limits.ask_inflight_hub = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
            _ => bad(
                errors,
                &field,
                "one of per_address_per_sec, per_identity_per_sec, ask_per_identity_per_10s, \
                 ask_per_address_per_10s, ask_inflight_per_identity, ask_inflight_hub",
            ),
        }
    }
}

fn check_relay(relay: &HubRelayFile, errors: &mut Vec<HubFieldError>) {
    if relay.bind.parse::<IpAddr>().is_err() {
        bad(errors, "relay.bind", "an IP address such as \"0.0.0.0\"");
    }
    if relay.port == 0 {
        bad(errors, "relay.port", "an integer 1..=65535");
    }
    let cap_max = crate::relay::RELAY_MAX_ALLOCATIONS;
    if relay.allocation_cap == 0 || usize::from(relay.allocation_cap) > cap_max {
        bad(
            errors,
            "relay.allocation_cap",
            &format!("an integer 1..={cap_max}"),
        );
    }
    // `relay.per_address_per_sec = 0` is no limit (`docs/formats/hub-toml.md`), which is what
    // the relay's source limiter does with 0.
}

fn check_path(field: &str, path: &str, errors: &mut Vec<HubFieldError>) {
    if path.is_empty() || path.contains('\0') {
        bad(errors, field, "a path");
    }
}

fn apply_relay(
    value: &toml::Value,
    relay: &mut HubRelayFile,
    saw_enabled: &mut bool,
    errors: &mut Vec<HubFieldError>,
) {
    let Some(table) = expect_table("relay", value, errors) else {
        return;
    };
    for (key, value) in table {
        let field = format!("relay.{key}");
        match key.as_str() {
            "enabled" => {
                if let Some(flag) = expect_bool(&field, value, errors) {
                    relay.enabled = flag;
                    *saw_enabled = true;
                }
            }
            "bind" => {
                if let Some(text) = expect_str(&field, value, errors) {
                    relay.bind = text;
                }
            }
            "port" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    relay.port = fit_u16(&field, n, errors).unwrap_or(relay.port);
                }
            }
            "allocation_cap" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    let cap_max = u64::try_from(crate::relay::RELAY_MAX_ALLOCATIONS).unwrap_or(u64::MAX);
                    if n == 0 || n > cap_max {
                        bad(errors, &field, &format!("an integer 1..={cap_max}"));
                    } else {
                        relay.allocation_cap = u16::try_from(n).unwrap_or(u16::MAX);
                    }
                }
            }
            "per_address_per_sec" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    relay.per_address_per_sec = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
            _ => bad(
                errors,
                &field,
                "one of enabled, bind, port, allocation_cap, per_address_per_sec",
            ),
        }
    }
}

fn apply_bans(value: &toml::Value, bans: &mut HubBansFile, errors: &mut Vec<HubFieldError>) {
    let Some(table) = expect_table("bans", value, errors) else {
        return;
    };
    for (key, value) in table {
        let field = format!("bans.{key}");
        match key.as_str() {
            "path" => {
                if let Some(text) = expect_str(&field, value, errors) {
                    bans.path = text;
                }
            }
            _ => bad(errors, &field, "path"),
        }
    }
}

fn apply_log(value: &toml::Value, log: &mut HubLogFile, errors: &mut Vec<HubFieldError>) {
    let Some(table) = expect_table("log", value, errors) else {
        return;
    };
    for (key, value) in table {
        let field = format!("log.{key}");
        match key.as_str() {
            "dir" => {
                if let Some(text) = expect_str(&field, value, errors) {
                    log.dir = text;
                }
            }
            _ => bad(errors, &field, "dir"),
        }
    }
}

fn apply_status(value: &toml::Value, status: &mut HubStatusFile, errors: &mut Vec<HubFieldError>) {
    let Some(table) = expect_table("status", value, errors) else {
        return;
    };
    for (key, value) in table {
        let field = format!("status.{key}");
        match key.as_str() {
            "enabled" => {
                if let Some(flag) = expect_bool(&field, value, errors) {
                    status.enabled = flag;
                }
            }
            "bind" => {
                if let Some(text) = expect_str(&field, value, errors) {
                    status.bind = text;
                }
            }
            "port" => {
                if let Some(n) = expect_u64(&field, value, errors) {
                    status.port = fit_u16(&field, n, errors).unwrap_or(status.port);
                }
            }
            _ => bad(errors, &field, "one of enabled, bind, port"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prints the schema hash the format table records. The assertion is the length; the line is
    /// the hash `docs/formats/format-hashes.ron` must carry.
    #[test]
    fn hub_toml_schema_hash_for_the_format_table() {
        let hex = blake3::hash(hub_toml_schema_fingerprint().as_bytes())
            .to_hex()
            .to_string();
        eprintln!("HUB_TOML_SCHEMA_HASH {hex}");
        assert_eq!(hex.len(), 64, "{hex}");
    }

    /// A v1 file that omits the ask keys keeps the wire caps. A value above the cap is accepted
    /// and clamped by [`HubFile::settings`]. Zero is a field error.
    #[test]
    fn ask_caps_default_and_clamp() {
        let file = HubFile::from_toml(DEFAULT_HUB_TOML).expect("default hub.toml");
        let settings = file.settings();
        assert_eq!(
            settings.ask_per_identity_per_10s,
            super::super::limits::ASK_PER_IDENTITY_CAP
        );
        assert_eq!(
            settings.ask_per_address_per_10s,
            super::super::limits::ASK_PER_ADDRESS_CAP
        );
        assert_eq!(
            settings.ask_inflight_per_identity,
            super::super::limits::ASK_INFLIGHT_PER_IDENTITY_CAP
        );
        assert_eq!(
            settings.ask_inflight_hub,
            super::super::limits::ASK_INFLIGHT_HUB_CAP
        );

        let text = "\
format_version = 1
[limits]
ask_inflight_hub = 2
ask_per_address_per_10s = 99
";
        let file = HubFile::from_toml(text).expect("optional ask keys");
        assert_eq!(file.limits.ask_inflight_hub, 2);
        assert_eq!(file.limits.ask_per_address_per_10s, 99);
        let settings = file.settings();
        assert_eq!(settings.ask_inflight_hub, 2);
        assert_eq!(
            settings.ask_per_address_per_10s,
            super::super::limits::ASK_PER_ADDRESS_CAP
        );

        let zero = HubFile::from_toml("format_version = 1\n[limits]\nask_inflight_hub = 0\n");
        let err = zero.expect_err("zero is refused");
        assert!(
            err.iter().any(|e| e.field == "limits.ask_inflight_hub"),
            "{err:?}"
        );
    }

    /// A fresh file enables the relay. A v1 file leaves it off. The migrator appends
    /// `enabled = false` and a second pass changes nothing.
    #[test]
    fn v2_defaults_on_and_v1_migrates_with_the_relay_off() {
        let fresh = HubDocument::from_toml(DEFAULT_HUB_TOML).expect("default");
        assert_eq!(fresh.file.format_version, 2);
        assert!(fresh.relay.enabled);
        assert_eq!(fresh.relay.port, DEFAULT_RELAY_PORT);
        assert_eq!(fresh.relay.allocation_cap, DEFAULT_RELAY_ALLOCATION_CAP);
        assert_eq!(fresh.relay.per_address_per_sec, 30);
        assert_eq!(fresh.bans.path, "hub-bans.txt");
        assert_eq!(fresh.log.dir, "logs");
        assert_eq!(
            fresh.relay_addr().expect("addr"),
            Some(std::net::SocketAddr::from(([0, 0, 0, 0], 7812)))
        );

        let v1 = "\
# Lobby hub (hub.toml v1). The key file is created on first run outside any project.
format_version = 1
bind = \"0.0.0.0\"
port = 7810
key_path = \"\"

[caps]
session_cap = 1024
lobby_cap = 256
max_members = 64
chat_per_sec = 4
handle_max_bytes = 24
idle_ms = 30000

[limits]
per_address_per_sec = 30
per_identity_per_sec = 20

[status]
enabled = true
bind = \"127.0.0.1\"
port = 7811
";
        let loaded = HubDocument::from_toml(v1).expect("v1 still loads");
        assert!(!loaded.relay.enabled);
        assert!(loaded.relay_addr().expect("addr").is_none());
        let migrated = migrate_hub_toml(v1).expect("migrate");
        assert!(migrated.contains("format_version = 2"), "{migrated}");
        assert!(migrated.contains("enabled = false"), "{migrated}");
        assert!(migrated.contains("session_cap = 1024"), "{migrated}");
        let doc = HubDocument::from_toml(&migrated).expect("migrated loads");
        assert!(!doc.relay.enabled);
        assert_eq!(doc.file.port, 7810);
        assert_eq!(migrate_hub_toml(&migrated).expect("idempotent"), migrated);

        let future = HubFile::from_toml("format_version = 3\n");
        let err = future.expect_err("v3 is refused");
        let text = err[0].to_string();
        assert!(text.contains("version"), "{text}");
        assert!(text.contains('3'), "{text}");
    }

    /// `docs/formats/hub-toml.md`: `relay.per_address_per_sec` of 0 means no limit, which is
    /// what [`crate::relay::RelayCore::set_source_rate`] already does. The parser rejects 0.
    #[test]
    fn relay_per_address_zero_means_no_limit() {
        let text = "\
format_version = 2
[relay]
enabled = false
per_address_per_sec = 0
";
        let doc = HubDocument::from_toml(text)
            .unwrap_or_else(|errors| panic!("relay.per_address_per_sec = 0 means no limit, got {errors:?}"));
        assert_eq!(doc.relay.per_address_per_sec, 0);
        assert!(!doc.relay.enabled);
    }

    /// A version-1 file that already has `[relay]` but no `enabled` key loads with the relay
    /// off. Migration must not turn it on: the version bump would, because a version-2 file
    /// that omits `enabled` defaults it to true, and the migrator does not write `false` when
    /// the table is already present.
    #[test]
    fn migrating_a_v1_relay_table_without_enabled_leaves_the_relay_off() {
        let v1 = "\
format_version = 1
port = 9000

[relay]
bind = \"127.0.0.1\"
port = 9001
allocation_cap = 4
";
        let loaded = HubDocument::from_toml(v1).expect("v1 with a relay table still loads");
        assert_eq!(loaded.file.format_version, 1);
        assert_eq!(loaded.file.port, 9000);
        assert!(
            !loaded.relay.enabled,
            "version 1 with no enabled key leaves the relay off"
        );
        assert_eq!(loaded.relay.port, 9001);
        assert_eq!(loaded.relay.allocation_cap, 4);

        let migrated = migrate_hub_toml(v1).expect("migrates");
        let doc = HubDocument::from_toml(&migrated).expect("migrated loads");
        assert_eq!(doc.file.format_version, 2);
        assert_eq!(doc.file.port, 9000, "{migrated}");
        assert_eq!(doc.relay.port, 9001, "{migrated}");
        assert_eq!(doc.relay.allocation_cap, 4, "{migrated}");
        assert!(!doc.relay.enabled, "migration turned the relay on:\n{migrated}");
        assert_eq!(migrate_hub_toml(&migrated).expect("idempotent"), migrated);
    }

    /// TOML integers include a leading `+` and hex. A version-1 file spelled that way still
    /// loads. The migrator only rewrites a run of decimal digits, so these spellings either
    /// fail to migrate or rewrite the version into a value other than 2.
    #[test]
    fn migrate_preserves_every_toml_spelling_of_version_1() {
        for v1 in [
            "format_version = +1\nport = 9000\n",
            "format_version = 0x1\nport = 9000\n",
        ] {
            let loaded = HubDocument::from_toml(v1)
                .unwrap_or_else(|errors| panic!("version 1 spelled `{v1}` still loads, got {errors:?}"));
            assert_eq!(loaded.file.format_version, 1, "{v1}");
            assert_eq!(loaded.file.port, 9000, "{v1}");
            assert!(!loaded.relay.enabled, "{v1}");
            let migrated = migrate_hub_toml(v1)
                .unwrap_or_else(|err| panic!("version 1 spelled `{v1}` migrates, got {err}"));
            let doc = HubDocument::from_toml(&migrated)
                .unwrap_or_else(|errors| panic!("migrated `{v1}` loads, got {errors:?}\n{migrated}"));
            assert_eq!(doc.file.format_version, 2, "{migrated}");
            assert_eq!(doc.file.port, 9000, "{migrated}");
            assert!(!doc.relay.enabled, "{migrated}");
        }
    }
}
