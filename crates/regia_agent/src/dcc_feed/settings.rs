// SPDX-License-Identifier: MPL-2.0
//! `Regia.toml [dcc_feed]` (`specs/60` § 3.2): the feed's settings, with defaults when the section
//! is absent (no migration: a new optional section).
//!
//! ```toml
//! [dcc_feed]
//! enabled = true
//! listen = ["unix", "tcp:127.0.0.1:0"]
//! auto_accept = ["my-blender-addon"]
//! playout = "delayed"
//! playout_delay_ms = 50
//!
//! [dcc_feed.limits]
//! max_vertices = 500000
//! ```

use std::net::SocketAddr;
use std::path::Path;

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use super::wire::Limits;

/// Largest `delay_queue_len` (spec § 2.6).
pub const MAX_DELAY_QUEUE_LEN: u32 = 1024;

/// Largest `window_bytes`: credits stay far from the `u32` of the wire's `ack`.
pub const MAX_WINDOW_BYTES: u32 = 1 << 30;

/// How the feed applies frames (spec § 2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Playout {
    /// Newest frame of each item at the next `PreUpdate`; no interpolation.
    #[default]
    Snap,
    /// Buffer frames and interpolate `playout_delay_ms` behind engine time.
    Delayed,
}

impl Playout {
    /// The `Regia.toml` / agent spelling.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Snap => "snap",
            Self::Delayed => "delayed",
        }
    }
}

/// One listener the server opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenSpec {
    /// The Unix domain socket `.regia/dcc_feed/feed.sock` (Linux, macOS). Ignored elsewhere.
    Unix,
    /// A TCP listener; port 0 picks a free port (written to `endpoint.json`).
    Tcp(SocketAddr),
    /// A WebSocket listener (binary and JSON subprotocols, spec § 4.2).
    WebSocket(SocketAddr),
}

impl ListenSpec {
    /// Parses `"unix"`, `"tcp:<ip>:<port>"` or `"ws:<ip>:<port>"`.
    ///
    /// # Errors
    ///
    /// A message naming the bad entry.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text == "unix" {
            return Ok(Self::Unix);
        }
        if let Some(addr) = text.strip_prefix("tcp:") {
            return addr
                .parse::<SocketAddr>()
                .map(Self::Tcp)
                .map_err(|e| format!("`listen` entry {text:?}: {e}"));
        }
        if let Some(addr) = text.strip_prefix("ws:") {
            return addr
                .parse::<SocketAddr>()
                .map(Self::WebSocket)
                .map_err(|e| format!("`listen` entry {text:?}: {e}"));
        }
        Err(format!(
            "`listen` entry {text:?}: expected \"unix\", \"tcp:<ip>:<port>\" or \"ws:<ip>:<port>\""
        ))
    }
}

/// Optional overrides of the wire [`Limits`] (`[dcc_feed.limits]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitOverrides {
    /// Largest reassembled message in bytes.
    pub max_message_bytes: Option<u32>,
    /// Most vertices in one mesh.
    pub max_vertices: Option<u32>,
    /// Most indices in one mesh.
    pub max_indices: Option<u32>,
    /// Most items per session.
    pub max_items: Option<u32>,
    /// Most joints per skeleton.
    pub max_joints: Option<u32>,
    /// Longest texture side.
    pub max_texture_dim: Option<u32>,
    /// Longest item key in bytes.
    pub max_key_len: Option<u32>,
}

impl LimitOverrides {
    /// The default limits with these overrides applied.
    #[must_use]
    pub fn apply(&self) -> Limits {
        let mut l = Limits::default();
        let set = |v: Option<u32>, slot: &mut u32| {
            if let Some(v) = v {
                *slot = v;
            }
        };
        set(self.max_message_bytes, &mut l.max_message_bytes);
        set(self.max_vertices, &mut l.max_vertices);
        set(self.max_indices, &mut l.max_indices);
        set(self.max_items, &mut l.max_items);
        set(self.max_joints, &mut l.max_joints);
        set(self.max_texture_dim, &mut l.max_texture_dim);
        set(self.max_key_len, &mut l.max_key_len);
        l
    }
}

/// The feed's settings (`Regia.toml [dcc_feed]`, spec § 3.2). Every field has a default, so an
/// absent section means "off".
#[derive(Resource, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DccFeedSettings {
    /// Whether the server listens at all (default false).
    pub enabled: bool,
    /// Listeners: `"unix"`, `"tcp:<ip>:<port>"`, `"ws:<ip>:<port>"` (default: the socket and TCP
    /// on `127.0.0.1:0`; a browser tool adds `"ws:127.0.0.1:0"`).
    pub listen: Vec<String>,
    /// `Origin` values a WebSocket upgrade may carry (default none: a browser page is refused).
    pub allowed_origins: Vec<String>,
    /// Allows a non-loopback TCP address (default false; the protocol has no TLS).
    pub allow_remote: bool,
    /// Most sessions at once (pending and active).
    pub max_sessions: u32,
    /// `snap` (default) or `delayed`.
    pub playout: Playout,
    /// Delay behind engine time in `delayed` playout.
    pub playout_delay_ms: u32,
    /// Frames buffered per item in `delayed` playout (cap [`MAX_DELAY_QUEUE_LEN`]).
    pub delay_queue_len: u32,
    /// Frames older than this are evicted from the playout buffer.
    pub max_frame_age_ms: u32,
    /// Engine time without a frame before an item is `stalled`.
    pub stall_after_ms: u32,
    /// Client names accepted without a prompt.
    pub auto_accept: Vec<String>,
    /// Byte window per session (`hello_ack.window_bytes`).
    pub window_bytes: u32,
    /// Overrides of the wire limits.
    pub limits: LimitOverrides,
}

impl Default for DccFeedSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: vec!["unix".to_owned(), "tcp:127.0.0.1:0".to_owned()],
            allowed_origins: Vec::new(),
            allow_remote: false,
            max_sessions: 4,
            playout: Playout::Snap,
            playout_delay_ms: 50,
            delay_queue_len: 64,
            max_frame_age_ms: 2000,
            stall_after_ms: 500,
            auto_accept: Vec::new(),
            window_bytes: 32 << 20,
            limits: LimitOverrides::default(),
        }
    }
}

impl DccFeedSettings {
    /// Reads `[dcc_feed]` from the text of a `Regia.toml`; defaults when the section is absent.
    ///
    /// # Errors
    ///
    /// The TOML parse or field error, or a [`Self::validate`] failure.
    pub fn from_manifest_text(text: &str) -> Result<Self, String> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| e.message().to_owned())?;
        let settings = match table.get("dcc_feed") {
            None => Self::default(),
            Some(section) => section
                .clone()
                .try_into::<Self>()
                .map_err(|e| format!("[dcc_feed]: {}", e.message()))?,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Reads the project's manifest and the remembered clients of `.regia/dcc_feed.toml`.
    /// A project without a manifest gets the defaults.
    ///
    /// # Errors
    ///
    /// As [`Self::from_manifest_text`], or an unreadable manifest.
    pub fn load(project_dir: &Path) -> Result<Self, String> {
        let mut settings = match regia_core::brand::project_manifest(project_dir) {
            None => Self::default(),
            Some(path) => {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                Self::from_manifest_text(&text)?
            }
        };
        for name in remembered_clients(project_dir) {
            if !settings.auto_accept.contains(&name) {
                settings.auto_accept.push(name);
            }
        }
        Ok(settings)
    }

    /// Checks ranges and listeners: a non-loopback TCP address needs `allow_remote = true`.
    ///
    /// # Errors
    ///
    /// A message naming the bad field.
    pub fn validate(&self) -> Result<(), String> {
        for entry in &self.listen {
            if let ListenSpec::Tcp(addr) | ListenSpec::WebSocket(addr) = ListenSpec::parse(entry)?
                && !addr.ip().is_loopback()
                && !self.allow_remote
            {
                return Err(format!(
                    "`listen` entry {entry:?} is not a loopback address; set allow_remote = true \
                     (the protocol has no TLS: use an SSH tunnel or a VPN)"
                ));
            }
        }
        if self.max_sessions == 0 {
            return Err("`max_sessions` must be at least 1".to_owned());
        }
        if self.delay_queue_len == 0 || self.delay_queue_len > MAX_DELAY_QUEUE_LEN {
            return Err(format!("`delay_queue_len` must be 1..={MAX_DELAY_QUEUE_LEN}"));
        }
        if !(1024..=MAX_WINDOW_BYTES).contains(&self.window_bytes) {
            return Err(format!("`window_bytes` must be 1024..={MAX_WINDOW_BYTES}"));
        }
        Ok(())
    }

    /// The parsed listeners.
    ///
    /// # Errors
    ///
    /// The first entry that does not parse.
    pub fn listeners(&self) -> Result<Vec<ListenSpec>, String> {
        self.listen.iter().map(|s| ListenSpec::parse(s)).collect()
    }

    /// The wire limits with the overrides applied.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits.apply()
    }

    /// The settings as `dcc_feed.status` reports them.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// The file holding remembered clients: `<project>/.regia/dcc_feed.toml`.
#[must_use]
pub fn remembered_path(project_dir: &Path) -> std::path::PathBuf {
    regia_core::brand::project_state_dir(project_dir).join("dcc_feed.toml")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Remembered {
    #[serde(default)]
    auto_accept: Vec<String>,
}

/// Client names remembered by `dcc_feed.accept { remember: true }` (empty when the file is absent
/// or unreadable: remembering is a convenience, never a reason to refuse to start).
#[must_use]
pub fn remembered_clients(project_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(remembered_path(project_dir))
        .ok()
        .and_then(|text| toml::from_str::<Remembered>(&text).ok())
        .map(|r| r.auto_accept)
        .unwrap_or_default()
}

/// Appends `client` to the remembered clients (atomic write: temp file, then rename).
///
/// # Errors
///
/// The I/O error text.
pub fn remember_client(project_dir: &Path, client: &str) -> Result<(), String> {
    let mut names = remembered_clients(project_dir);
    if names.iter().any(|n| n == client) {
        return Ok(());
    }
    names.push(client.to_owned());
    let text = toml::to_string(&Remembered { auto_accept: names }).map_err(|e| e.to_string())?;
    let path = remembered_path(project_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    // Owner-only temp file + rename: a symlink planted at a fixed temp name is replaced,
    // not followed (the same hardening as the endpoint token files).
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    crate::endpoint::write_owner_only(dir, "dcc_feed.toml", text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_is_the_defaults_and_off() {
        let s = DccFeedSettings::from_manifest_text("[project]\nname = \"x\"\n").expect("parses");
        assert_eq!(s, DccFeedSettings::default());
        assert!(!s.enabled);
        assert_eq!(s.max_sessions, 4);
        assert_eq!(s.playout, Playout::Snap);
        assert_eq!(
            s.listeners().expect("listen"),
            vec![
                ListenSpec::Unix,
                ListenSpec::Tcp("127.0.0.1:0".parse().expect("addr"))
            ]
        );
    }

    #[test]
    fn section_fields_and_limit_overrides_apply() {
        let text = "[dcc_feed]\nenabled = true\nplayout = \"delayed\"\nauto_accept = [\"a\"]\n\
                    [dcc_feed.limits]\nmax_vertices = 10\n";
        let s = DccFeedSettings::from_manifest_text(text).expect("parses");
        assert!(s.enabled);
        assert_eq!(s.playout, Playout::Delayed);
        assert_eq!(s.auto_accept, vec!["a".to_owned()]);
        assert_eq!(s.limits().max_vertices, 10);
        assert_eq!(s.limits().max_indices, Limits::default().max_indices);
    }

    #[test]
    fn a_remote_bind_needs_allow_remote() {
        let text = "[dcc_feed]\nlisten = [\"tcp:0.0.0.0:7000\"]\n";
        let err = DccFeedSettings::from_manifest_text(text).expect_err("refused");
        assert!(err.contains("allow_remote"), "{err}");
        let text = "[dcc_feed]\nlisten = [\"tcp:0.0.0.0:7000\"]\nallow_remote = true\n";
        assert!(DccFeedSettings::from_manifest_text(text).is_ok());
    }

    #[test]
    fn bad_values_are_named() {
        for (text, needle) in [
            ("[dcc_feed]\nlisten = [\"pipe\"]\n", "listen"),
            ("[dcc_feed]\nmax_sessions = 0\n", "max_sessions"),
            ("[dcc_feed]\ndelay_queue_len = 5000\n", "delay_queue_len"),
            ("[dcc_feed]\nbogus = 1\n", "bogus"),
            ("[dcc_feed]\nplayout = \"smooth\"\n", "smooth"),
        ] {
            let err = DccFeedSettings::from_manifest_text(text).expect_err(text);
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn remembered_clients_round_trip() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_remember");
        assert!(remembered_clients(&dir).is_empty());
        remember_client(&dir, "addon").expect("write");
        remember_client(&dir, "addon").expect("idempotent");
        remember_client(&dir, "other").expect("write");
        assert_eq!(
            remembered_clients(&dir),
            vec!["addon".to_owned(), "other".to_owned()]
        );
        let loaded = DccFeedSettings::load(&dir).expect("no manifest: defaults");
        assert_eq!(loaded.auto_accept, vec!["addon".to_owned(), "other".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_tmp_symlink_is_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_remember_link");
        let state = regia_core::brand::project_state_dir(&dir);
        std::fs::create_dir_all(&state).expect("state dir");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "precious").expect("victim");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        // The pre-fix temp name was the fixed sibling `dcc_feed.toml.tmp`, opened with
        // create + truncate: a planted symlink there truncated the target.
        std::os::unix::fs::symlink(&victim, state.join("dcc_feed.toml.tmp")).expect("plant link");
        remember_client(&dir, "addon").expect("write");
        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "precious");
        assert_eq!(
            std::fs::metadata(&victim)
                .expect("victim meta")
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "the write replaced the planted link instead of chmodding its target"
        );
        let dest = state.join("dcc_feed.toml");
        assert!(
            !std::fs::symlink_metadata(&dest)
                .expect("dest meta")
                .file_type()
                .is_symlink(),
            "the settings landed as a regular file, not a link"
        );
        assert_eq!(remembered_clients(&dir), vec!["addon".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
