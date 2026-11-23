// SPDX-License-Identifier: MPL-2.0
//! `hub.toml` for the `regia-lobby-hub` process (NL-06, ADR-8648).
//!
//! The file is read key by key. A bad value or an unknown key is a named field error
//! (`field: expected ...`), the same shape as `server.toml`. The server key path is created on
//! first run and is refused when it sits inside a project (a directory whose ancestor holds
//! `Regia.toml`). An empty `key_path` means the user config directory, never a path next to the
//! package.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use regia_core::schema::join_field_lists;

use super::HubSettings;

/// The `format_version` this build writes and reads.
pub const HUB_TOML_FORMAT_VERSION: u32 = 1;

/// Default UDP listen port (the DO27 hub listen default, kept as a setting).
pub const DEFAULT_HUB_PORT: u16 = 7810;

/// Default health-endpoint port, bound to loopback.
pub const DEFAULT_STATUS_PORT: u16 = 7811;

/// Requests one address may make per second, unless `hub.toml` says otherwise.
pub const DEFAULT_PER_ADDRESS_PER_SEC: u32 = 30;

/// Requests one player key may make per second, unless `hub.toml` says otherwise.
pub const DEFAULT_PER_IDENTITY_PER_SEC: u32 = 20;

/// Every key `hub.toml` v1 reads, in the order of `docs/formats/hub-toml.md`.
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
    "status.enabled",
    "status.bind",
    "status.port",
];

/// The schema fingerprint `cargo xtask check-formats` hashes (one field list).
#[must_use]
pub fn hub_toml_schema_fingerprint() -> String {
    join_field_lists(&[HUB_TOML_KEYS.iter().map(|k| (*k).to_owned()).collect()])
}

/// Versions this file migrates from. v1 is the first version, so the list is empty.
#[must_use]
pub fn hub_toml_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The `hub.toml` a package stages and a process with no file uses.
pub const DEFAULT_HUB_TOML: &str = "\
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

/// Rate limits that sit in front of the hub core, including its member-data limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubLimits {
    /// Requests per second from one IP address.
    pub per_address_per_sec: u32,
    /// Requests per second from one player key.
    pub per_identity_per_sec: u32,
}

impl Default for HubLimits {
    fn default() -> Self {
        Self {
            per_address_per_sec: DEFAULT_PER_ADDRESS_PER_SEC,
            per_identity_per_sec: DEFAULT_PER_IDENTITY_PER_SEC,
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
    /// # Errors
    ///
    /// One [`HubFieldError`] per bad or unknown field. The display is `field: expected ...`.
    pub fn from_toml(text: &str) -> Result<Self, Vec<HubFieldError>> {
        // A missing file uses the built-in document. A blank file is not that document: the fuzz
        // harness rejects empty text, and a cut through a quoted value must fail the parse.
        if text.trim().is_empty() {
            return Err(vec![HubFieldError {
                field: "hub.toml".into(),
                expected: "a hub.toml document".into(),
            }]);
        }
        let mut file = Self::default();
        let mut errors = Vec::new();
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
                    if let Some(n) = expect_u64("format_version", value, &mut errors) {
                        if n == u64::from(HUB_TOML_FORMAT_VERSION) {
                            file.format_version = HUB_TOML_FORMAT_VERSION;
                        } else {
                            bad(
                                &mut errors,
                                "format_version",
                                &format!("{HUB_TOML_FORMAT_VERSION}, found {n}"),
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
                other => bad(
                    &mut errors,
                    other,
                    "one of format_version, bind, port, key_path, caps, limits, status",
                ),
            }
        }
        if errors.is_empty() {
            file.check_ranges(&mut errors);
        }
        if errors.is_empty() { Ok(file) } else { Err(errors) }
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
            _ => bad(errors, &field, "one of per_address_per_sec, per_identity_per_sec"),
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
}
