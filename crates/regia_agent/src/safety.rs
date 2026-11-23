// SPDX-License-Identifier: MPL-2.0
//! Safety basics of `docs/specs/23-agent-native.md` § 5: permission profiles
//! (`.regia/agent.toml`), read-only mode, the per-call audit log
//! (`.regia/agent/audit.jsonl`) and the provenance records of agent-made changes
//! (`agent.changes`).
//!
//! What is deliberately *not* here: staged review and asset `.meta` provenance live with the
//! editor's review queue (specs/23 § 2, editor lane); this crate records world changes only.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::schema::Permission;

/// The file a project uses to gate the agent: `<project>/.regia/agent.toml`.
pub const AGENT_TOML: &str = "agent.toml";

/// Parsed `.regia/agent.toml` (unknown keys are ignored for forward compatibility).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// Reject every state-changing call.
    pub read_only: bool,
    /// Granted permission names (`read`, `edit_scene`, `play`, `assets`).
    pub permissions: BTreeSet<String>,
    /// Mutating calls allowed per second.
    pub rate_limit_per_second: u32,
    /// Heavy calls (whole-world walks, image encode or decode, multi-frame captures; the
    /// method contract's `cost`) allowed per second, read-only ones included (ADR-1034).
    pub heavy_rate_limit_per_second: u32,
    /// Extra absolute directories `render.out_start` may write under, on top of the project
    /// root, the system temp dir.
    pub output_roots: Vec<PathBuf>,
    /// Extra absolute directories `replay.load` may read a replay from (a replay store), on top
    /// of the project root, the system temp dir and `output_roots`. Read only:
    /// nothing is ever written here (ADR-1965). The host's [`crate::ReplayRoots`] adds more.
    pub replay_roots: Vec<PathBuf>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        // Default profile (specs/23 § 5): read, edit_scene, assets, play.
        // build, tools and review are granted only when agent.toml lists them.
        Self {
            read_only: false,
            permissions: ["read", "edit_scene", "assets", "play"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            rate_limit_per_second: 50,
            heavy_rate_limit_per_second: 60,
            output_roots: Vec::new(),
            replay_roots: Vec::new(),
        }
    }
}

impl AgentConfig {
    /// The fail-closed profile used when `agent.toml` exists but cannot be read or parsed:
    /// `read_only` with the `read` permission only.
    #[must_use]
    pub fn fail_closed() -> Self {
        Self {
            read_only: true,
            permissions: std::iter::once("read".to_string()).collect(),
            rate_limit_per_second: Self::default().rate_limit_per_second,
            heavy_rate_limit_per_second: Self::default().heavy_rate_limit_per_second,
            output_roots: Vec::new(),
            replay_roots: Vec::new(),
        }
    }

    /// Reads `<project>/.regia/agent.toml` when present; a missing file → the default profile.
    /// A file that exists but cannot be read or parsed fails closed ([`Self::fail_closed`]),
    /// logged, never fatal: the agent server must still come up.
    pub fn load(project_dir: &Path) -> Self {
        let path = regia_core::brand::project_state_dir(project_dir).join(AGENT_TOML);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                tracing::error!(
                    target: "regia_agent",
                    "cannot read {}: {err} — failing closed (read only)",
                    path.display()
                );
                return Self::fail_closed();
            }
        };
        match Self::parse(&text) {
            Ok(config) => config,
            Err(err) => {
                tracing::error!(
                    target: "regia_agent",
                    "invalid {}: {err} — failing closed (read only)",
                    path.display()
                );
                Self::fail_closed()
            }
        }
    }

    /// Parses agent.toml text. `read_only = true` forces every permission off except read.
    fn parse(text: &str) -> Result<Self, String> {
        #[derive(Deserialize, Default)]
        struct Raw {
            read_only: Option<bool>,
            permissions: Option<Vec<String>>,
            rate_limit_per_second: Option<u32>,
            heavy_rate_limit_per_second: Option<u32>,
            output_roots: Option<Vec<PathBuf>>,
            replay_roots: Option<Vec<PathBuf>>,
        }
        let raw: Raw = toml::from_str(text).map_err(|e| format!("TOML parse error: {e}"))?;
        let read_only = raw.read_only.unwrap_or(false);
        let permissions: BTreeSet<String> = raw
            .permissions
            .unwrap_or_else(|| Self::default().permissions.into_iter().collect())
            .into_iter()
            .collect();
        Ok(Self {
            read_only,
            permissions,
            rate_limit_per_second: raw
                .rate_limit_per_second
                .unwrap_or_else(|| Self::default().rate_limit_per_second)
                .clamp(1, 10_000),
            heavy_rate_limit_per_second: raw
                .heavy_rate_limit_per_second
                .unwrap_or_else(|| Self::default().heavy_rate_limit_per_second)
                .clamp(1, 10_000),
            output_roots: raw.output_roots.unwrap_or_default(),
            replay_roots: raw.replay_roots.unwrap_or_default(),
        })
    }
}

/// The project's agent permissions, as a resource. [`crate::AgentPlugin`] loads them in
/// `finish`; tests insert the resource directly.
#[derive(Resource, Debug, Clone, Default)]
pub struct AgentPermissions {
    /// The loaded configuration.
    pub config: AgentConfig,
    /// Where the configuration came from (absent when the default profile was used).
    pub source: Option<PathBuf>,
}

impl AgentPermissions {
    /// Loads the project's profile (`.regia/agent.toml`), remembering where it came from.
    pub fn load(project_dir: &Path) -> Self {
        let config = AgentConfig::load(project_dir);
        Self {
            source: Some(regia_core::brand::project_state_dir(project_dir).join(AGENT_TOML)),
            config,
        }
    }

    /// Whether a method with this permission may run.
    pub fn allows(&self, permission: Permission) -> bool {
        if self.config.read_only && permission != Permission::Read {
            return false;
        }
        self.config.permissions.contains(permission.name())
    }

    /// The stable error for a denied call (JSON-RPC `PERMISSION_DENIED`, code `F0012`).
    pub fn denied_error(&self, permission: Permission) -> crate::protocol::RpcError {
        let fix = if self.config.read_only {
            format!(
                "this app runs with read_only = true in .regia/{AGENT_TOML}; only read-only \
                 methods are allowed"
            )
        } else {
            format!(
                "add \"{}\" to the permissions list in .regia/{AGENT_TOML}",
                permission.name()
            )
        };
        crate::protocol::RpcError::new(
            crate::protocol::PERMISSION_DENIED,
            "F0012",
            format!("this call needs the \"{}\" permission", permission.name()),
            fix,
        )
    }
}

/// Who is calling: reported by `agent.hello`, stamped onto every provenance record
/// (specs/23 § 5: client, session, model).
#[derive(Resource, Debug, Clone)]
pub struct AgentSession {
    /// Client name, e.g. `regia-mcp` or `cargo test`.
    pub client: String,
    /// Model name, when the client reports one.
    pub model: String,
    /// Session id (this app process).
    pub session: String,
}

impl Default for AgentSession {
    fn default() -> Self {
        Self {
            client: "unknown".into(),
            model: String::new(),
            session: format!("pid-{}", std::process::id()),
        }
    }
}

/// One audit entry (`.regia/agent/audit.jsonl`, specs/23 § 5): frame, time, method, params
/// hash, outcome, error code, duration and client. There is no result summary and no undo id.
#[derive(Debug)]
pub struct AuditEntry<'a> {
    /// Frame the call was handled in.
    pub frame: u64,
    /// Unix ms of the call.
    pub unix_ms: u64,
    /// Method name.
    pub method: &'a str,
    /// FNV-1a hash of the params (compact; params can be huge).
    pub params_hash: u64,
    /// Outcome.
    pub outcome: &'a str,
    /// Stable Regia Engine error code, when the call failed.
    pub error_code: Option<&'a str>,
    /// Handler duration in microseconds.
    pub duration_us: u64,
    /// Client name.
    pub client: &'a str,
}

/// Size at which the audit log moves to `audit.1.jsonl` and starts over. A read-only client looping
/// a call must not fill the disk; one previous file is kept.
pub const MAX_AUDIT_BYTES: u64 = 64 * 1024 * 1024;

/// The append-only audit log. IO failures are logged once and then ignored: auditing must
/// never take the protocol down.
#[derive(Resource, Default)]
pub struct AuditLog {
    file: Option<File>,
    /// Path of the log file (for `project.info`).
    pub path: Option<PathBuf>,
    seq: u64,
    warned: bool,
    /// Bytes in the open file.
    written: u64,
    /// Bytes at which it rotates.
    cap: u64,
}

impl AuditLog {
    /// Opens (or creates) the audit log for appending. A project is often a cloned repository: a
    /// link planted at the log's name is replaced, never appended through, and a linked agent
    /// folder turns the log off (`endpoint::prepare_dir`).
    pub fn open(path: PathBuf) -> Self {
        Self::open_with_cap(path, MAX_AUDIT_BYTES)
    }

    fn open_with_cap(path: PathBuf, cap: u64) -> Self {
        match Self::open_file(&path) {
            Ok((file, written)) => Self {
                file: Some(file),
                path: Some(path),
                seq: 0,
                warned: false,
                written,
                cap,
            },
            Err(err) => {
                tracing::warn!(target: "regia_agent", "cannot open {}: {err}", path.display());
                Self::default()
            }
        }
    }

    /// The log file for appending (mode 0600 on Unix) and its current size.
    fn open_file(path: &Path) -> std::io::Result<(File, u64)> {
        if let Some(dir) = path.parent() {
            crate::endpoint::prepare_dir(dir)?;
        }
        match path.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(path)?,
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the audit log's name is taken by something other than a file",
                ));
            }
            _ => {}
        }
        #[cfg(windows)]
        Self::make_owner_only_on_windows(path)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        let written = file.metadata()?.len();
        Ok((file, written))
    }

    /// Windows: a missing log is created empty with an owner-only DACL (`endpoint::create_owner_only`),
    /// and a log an earlier version left with its folder's inherited ACL (readable by every local
    /// account) is rewritten owner-only with the same lines (ADR-5489). The append-mode open that
    /// follows keeps the file's DACL.
    #[cfg(windows)]
    fn make_owner_only_on_windows(path: &Path) -> std::io::Result<()> {
        match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                drop(crate::endpoint::create_owner_only(path)?);
                Ok(())
            }
            Err(e) => Err(e),
            Ok(meta) => {
                if !regia_net::private_file::exposed_to_other_users(path, &meta)
                    .map_err(std::io::Error::other)?
                {
                    return Ok(());
                }
                let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
                else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "the audit log has no folder or no UTF-8 name",
                    ));
                };
                let lines = std::fs::read(path)?;
                crate::endpoint::write_owner_only(dir, name, &lines)
            }
        }
    }

    /// Moves a full log to `audit.1.jsonl` (a rename: a link at that name is replaced) and starts a
    /// new one. When the rename fails the log keeps appending to the old file rather than losing
    /// lines. When the rename worked but the new log cannot be opened, the open handle is the
    /// rotated file: appending to it would grow `audit.1.jsonl` past every cap, so the log stops
    /// (as a linked agent folder turns it off at open).
    fn rotate(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let previous = path.with_file_name("audit.1.jsonl");
        if let Err(err) = std::fs::rename(&path, &previous) {
            tracing::warn!(target: "regia_agent", "audit log rotation failed: {err}");
            // Do not retry on every line.
            self.written = 0;
            return;
        }
        match Self::open_file(&path) {
            Ok((file, written)) => {
                self.file = Some(file);
                self.written = written;
            }
            Err(err) => {
                tracing::warn!(
                    target: "regia_agent",
                    "audit log rotated but the new log cannot be opened, auditing stops: {err}"
                );
                self.file = None;
                self.written = 0;
            }
        }
    }

    /// Appends one entry as a JSON line. Errors are logged (once) and swallowed.
    pub fn record(&mut self, entry: &AuditEntry<'_>) {
        // The log numbers its own lines (1-based, in write order).
        self.seq += 1;
        let line = json!({
            "seq": self.seq,
            "frame": entry.frame,
            "unix_ms": entry.unix_ms,
            "method": entry.method,
            "params_hash": format!("{:016x}", entry.params_hash),
            "outcome": entry.outcome,
            "error_code": entry.error_code,
            "duration_us": entry.duration_us,
            "client": entry.client,
        });
        if self.file.is_some() && self.written >= self.cap {
            self.rotate();
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let mut line = line.to_string();
        line.push('\n');
        self.written = self.written.saturating_add(line.len() as u64);
        if let Err(err) = file.write_all(line.as_bytes())
            && !self.warned
        {
            tracing::warn!(target: "regia_agent", "audit log write failed: {err}");
            self.warned = true;
        }
    }
}

/// One provenance record of an agent-made change (specs/23 § 5, adapted to world changes:
/// the editor's staged review queue covers asset edits).
#[derive(Debug, Clone)]
pub struct ChangeRecord {
    /// Sequence number.
    pub seq: u64,
    /// Frame the change happened in.
    pub frame: u64,
    /// Unix ms of the change.
    pub unix_ms: u64,
    /// Method that made it.
    pub method: String,
    /// What it touched (`entity 42`, `resource GameSettings.audio.master`, …).
    pub target: String,
    /// Compact summary of the change (before/after values).
    pub summary: Value,
    /// Client that made the change.
    pub client: String,
    /// Model, when reported.
    pub model: String,
    /// Session id of the app process.
    pub session: String,
}

/// The in-memory provenance log of this session, read back through `agent.changes`.
#[derive(Resource, Default)]
pub struct ChangeLog {
    /// Records, oldest first.
    pub records: Vec<ChangeRecord>,
    seq: u64,
}

/// Most change records kept in memory; older ones drop off (`seq` keeps counting, so
/// `agent.changes { since }` cursors stay valid).
pub const MAX_CHANGE_RECORDS: usize = 10_000;

impl ChangeLog {
    /// Records a successful mutating call.
    pub fn push(
        &mut self,
        frame: u64,
        method: &str,
        target: String,
        summary: Value,
        session: &AgentSession,
    ) -> u64 {
        self.seq += 1;
        if self.records.len() >= MAX_CHANGE_RECORDS {
            self.records.drain(..MAX_CHANGE_RECORDS / 10);
        }
        self.records.push(ChangeRecord {
            seq: self.seq,
            frame,
            unix_ms: unix_ms_now(),
            method: method.to_string(),
            target,
            summary,
            client: session.client.clone(),
            model: session.model.clone(),
            session: session.session.clone(),
        });
        self.seq
    }
}

/// The audit log's `params_hash` of a request's params: FNV-1a 64-bit (public-domain algorithm;
/// Glenn Fowler et al., 1991) over their compact JSON text, the bytes `serde_json::to_string`
/// gives. The text is streamed into the hash as it is serialized, so hashing allocates nothing
/// however large the params are (ADR-7454). A host can call it to match its own calls to
/// audit lines.
///
/// # Example
///
/// ```
/// let hash = regia_agent::params_hash(&serde_json::json!({ "name": "Rock" }));
/// assert_eq!(hash, regia_agent::params_hash(&serde_json::json!({ "name": "Rock" })));
/// ```
#[must_use]
pub fn params_hash(params: &Value) -> u64 {
    let mut hasher = Fnv1a(FNV_OFFSET_BASIS);
    match serde_json::to_writer(&mut hasher, params) {
        Ok(()) => hasher.0,
        // The string form hashed "" when serialization failed (a `Value` always serializes).
        Err(_) => FNV_OFFSET_BASIS,
    }
}

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// An `io::Write` that folds every byte written into an FNV-1a 64-bit state.
struct Fnv1a(u64);

impl Fnv1a {
    fn fold(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
}

impl std::io::Write for Fnv1a {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.fold(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// FNV-1a 64-bit over bytes (basis `0xcbf29ce484222325`, prime `0x00000100000001b3`).
#[cfg(test)]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hasher = Fnv1a(FNV_OFFSET_BASIS);
    hasher.fold(bytes);
    hasher.0
}

/// Current time as Unix milliseconds (0 when the clock is unavailable — never fails).
pub fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl AgentConfig {
    /// Whether the named permission is granted (used by tests and tooling).
    #[cfg(test)]
    pub fn allows_str(&self, name: &str) -> bool {
        self.permissions.contains(name)
    }
}

#[cfg(test)]
mod tests {
    fn audit_entry() -> AuditEntry<'static> {
        AuditEntry {
            frame: 1,
            unix_ms: 2,
            method: "agent.hello",
            params_hash: 3,
            outcome: "ok",
            error_code: None,
            duration_us: 4,
            client: "$(touch pwned)",
        }
    }

    /// Audit ADR-3206 finding 1: `audit.jsonl -> ~/.bashrc` in a cloned project appended caller-chosen
    /// JSON lines to that file on every call. The link is replaced; its target stays as it was.
    #[cfg(unix)]
    #[test]
    fn the_audit_log_replaces_a_planted_link_and_never_appends_through_it() {
        let base = regia_test_utils::unique_temp_dir("audit-link");
        let dir = base.join("agent");
        std::fs::create_dir_all(&dir).expect("dir");
        for (name, target_exists) in [("present", true), ("dangling", false)] {
            let target = base.join(name);
            if target_exists {
                std::fs::write(&target, "export KEEP=1\n").expect("target");
            }
            let path = dir.join("audit.jsonl");
            let _ = std::fs::remove_file(&path);
            std::os::unix::fs::symlink(&target, &path).expect("link");
            let mut log = AuditLog::open(path.clone());
            log.record(&audit_entry());
            drop(log);
            if target_exists {
                assert_eq!(
                    std::fs::read_to_string(&target).expect("target"),
                    "export KEEP=1\n"
                );
            } else {
                assert!(!target.exists(), "a dangling link created its target");
            }
            let meta = path.symlink_metadata().expect("log");
            assert!(meta.file_type().is_file(), "{name}");
            assert!(
                std::fs::read_to_string(&path)
                    .expect("log")
                    .contains("agent.hello")
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The log's folder being a link (finding 3) leaves it off, and nothing lands in the target.
    #[cfg(unix)]
    #[test]
    fn the_audit_log_in_a_linked_folder_is_off() {
        let base = regia_test_utils::unique_temp_dir("audit-link-dir");
        let target = base.join("victim");
        std::fs::create_dir_all(&target).expect("target");
        let dir = base.join("agent");
        std::os::unix::fs::symlink(&target, &dir).expect("link");
        let mut log = AuditLog::open(dir.join("audit.jsonl"));
        log.record(&audit_entry());
        assert!(log.path.is_none());
        assert_eq!(std::fs::read_dir(&target).expect("target").count(), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_audit_log_in_place_of_a_folder_is_off() {
        let base = regia_test_utils::unique_temp_dir("audit-a-dir");
        let path = base.join("audit.jsonl");
        std::fs::create_dir_all(&path).expect("a folder at the name");
        let mut log = AuditLog::open(path);
        log.record(&audit_entry());
        assert!(log.path.is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A FIFO at the log's name: opening it for writing would wait for a reader and stall the
    /// agent's start, so the log is off instead. The channel's timeout bounds a hang only.
    #[cfg(unix)]
    #[test]
    fn the_audit_log_in_place_of_a_pipe_is_off_without_blocking() {
        let base = regia_test_utils::unique_temp_dir("audit-a-pipe");
        let path = base.join("audit.jsonl");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .is_ok_and(|s| s.success());
        assert!(made, "mkfifo");
        let (tx, rx) = std::sync::mpsc::channel();
        let opened = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(AuditLog::open(opened).path.is_none());
        });
        let off = rx.recv_timeout(std::time::Duration::from_secs(120));
        if off.is_err() {
            // Lets the stuck open finish so the thread ends.
            let _ = std::fs::File::open(&path);
        }
        assert_eq!(off, Ok(true), "the log opened a pipe or waited on it");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit ADR-3206 finding 2: a client looping a read-only call grew the log without limit.
    #[test]
    fn the_audit_log_rotates_at_its_cap_and_counts_what_it_found() {
        let base = regia_test_utils::unique_temp_dir("audit-rotate");
        let path = base.join("audit.jsonl");
        let mut log = AuditLog::open_with_cap(path.clone(), 1);
        log.record(&audit_entry());
        assert!(!base.join("audit.1.jsonl").exists());
        log.record(&audit_entry());
        let previous = std::fs::read_to_string(base.join("audit.1.jsonl")).expect("rotated");
        assert_eq!(previous.lines().count(), 1);
        assert_eq!(std::fs::read_to_string(&path).expect("log").lines().count(), 1);
        drop(log);
        // A reopened log counts the size it found, so a restart does not reset the budget.
        let mut log = AuditLog::open_with_cap(path.clone(), 1);
        assert!(log.written > 0);
        log.record(&audit_entry());
        assert_eq!(std::fs::read_to_string(&path).expect("log").lines().count(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit corr-agent finding 17 (ADR-5895): when the rename to `audit.1.jsonl` worked but the new
    /// log could not be opened (here: the agent folder was swapped for a link, which turns the log
    /// off), the old handle kept appending to `audit.1.jsonl` with the count reset, so the cap
    /// never applied again and that file grew without bound. The log now stops instead.
    #[cfg(unix)]
    #[test]
    fn a_failed_reopen_after_rotation_stops_the_log_instead_of_growing_the_old_file() {
        let base = regia_test_utils::unique_temp_dir("audit-rotate-reopen");
        let dir = base.join("agent");
        std::fs::create_dir_all(&dir).expect("agent folder");
        let path = dir.join("audit.jsonl");
        let mut log = AuditLog::open_with_cap(path.clone(), 1);
        log.record(&audit_entry());
        // The folder moves and a link takes its name: the rename still resolves, the reopen is refused.
        let moved = base.join("agent-moved");
        std::fs::rename(&dir, &moved).expect("move the folder");
        std::os::unix::fs::symlink(&moved, &dir).expect("link");
        for _ in 0..5 {
            log.record(&audit_entry());
        }
        let rotated = std::fs::read_to_string(moved.join("audit.1.jsonl")).expect("rotated");
        assert_eq!(
            rotated.lines().count(),
            1,
            "only the line written before the cap: later lines went into the rotated file past its cap"
        );
        assert!(
            !moved.join("audit.jsonl").exists(),
            "no new log opens through the link"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_audit_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let base = regia_test_utils::unique_temp_dir("audit-mode");
        let path = base.join("audit.jsonl");
        let mut log = AuditLog::open(path.clone());
        log.record(&audit_entry());
        let mode = std::fs::metadata(&path).expect("log").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
        let _ = std::fs::remove_dir_all(&base);
    }

    use super::*;

    /// `docs/security/threat-model.md`: running code, approving an agent's own proposals and driving
    /// tools are grants a project makes on purpose; a project with no `agent.toml` has none of them.
    #[test]
    fn the_default_profile_holds_no_code_execution_permission() {
        let config = AgentConfig::default();
        for name in ["build", "tools", "review"] {
            assert!(!config.permissions.contains(name), "{name}");
        }
        let fail_closed = AgentConfig::fail_closed();
        assert_eq!(fail_closed.permissions.iter().collect::<Vec<_>>(), ["read"]);
    }

    #[test]
    fn change_log_is_trimmed_but_keeps_counting() {
        let mut log = ChangeLog::default();
        let session = AgentSession::default();
        for _ in 0..MAX_CHANGE_RECORDS + 5 {
            log.push(0, "scene.spawn", String::new(), Value::Null, &session);
        }
        assert!(log.records.len() <= MAX_CHANGE_RECORDS);
        let last = log.records.last().expect("records");
        assert_eq!(last.seq, (MAX_CHANGE_RECORDS + 5) as u64, "seq is never reused");
    }

    #[test]
    fn broken_agent_toml_fails_closed() {
        // audit-g-agent6-platform (Low): a pid-only fixed path broke the next run when a killed
        // run left a stale broken agent.toml behind (the file loads fail-closed read_only).
        let dir = regia_test_utils::unique_temp_dir("agent-toml-broken");
        std::fs::create_dir_all(dir.join(".regia")).expect("mkdir");
        // Missing file: the default profile.
        assert!(!AgentConfig::load(&dir).read_only);
        std::fs::write(dir.join(".regia").join(AGENT_TOML), "read_only = [oops").expect("write");
        let config = AgentConfig::load(&dir);
        assert!(config.read_only);
        assert_eq!(config.permissions.len(), 1);
        assert!(config.permissions.contains("read"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_toml_in_a_legacy_state_dir_still_applies() {
        // A project from before the rename (ADR-1388) keeps its profile in the legacy directory.
        let dir = regia_test_utils::unique_temp_dir("agent-toml-legacy");
        let legacy = dir.join(regia_core::brand::LEGACY_PROJECT_STATE_DIR);
        std::fs::create_dir_all(&legacy).expect("mkdir");
        std::fs::write(legacy.join(AGENT_TOML), "read_only = true\n").expect("write");
        assert!(AgentConfig::load(&dir).read_only, "the legacy profile is read");
        assert_eq!(crate::endpoint::agent_dir(&dir), legacy.join("agent"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_output_roots() {
        let config =
            AgentConfig::parse("output_roots = [\"/srv/renders\", \"/mnt/farm\"]\n").expect("parses");
        assert_eq!(
            config.output_roots,
            [PathBuf::from("/srv/renders"), PathBuf::from("/mnt/farm")]
        );
        assert!(
            AgentConfig::parse("output_roots = 3\n").is_err(),
            "a bad value fails closed in load"
        );
    }

    #[test]
    fn parses_replay_roots_apart_from_output_roots() {
        let config = AgentConfig::parse("replay_roots = [\"/srv/store\"]\n").expect("parses");
        assert_eq!(config.replay_roots, [PathBuf::from("/srv/store")]);
        assert!(
            config.output_roots.is_empty(),
            "a replay root is not a write root"
        );
        assert!(AgentConfig::default().replay_roots.is_empty());
        assert!(AgentConfig::fail_closed().replay_roots.is_empty());
        assert!(
            AgentConfig::parse("replay_roots = \"/srv/store\"\n").is_err(),
            "a bad value fails closed in load"
        );
    }

    #[test]
    fn published_permission_names_are_what_allows_checks() {
        assert_eq!(Permission::from_name("assets"), Some(Permission::Assets));
        assert_eq!(Permission::from_name("edit_assets"), None);
        assert_eq!(Permission::from_name("cook"), None);
        assert_eq!(Permission::from_name("package"), None);
        assert_eq!(Permission::from_name("delete"), None);
        let names: Vec<_> = AgentConfig::default().permissions.into_iter().collect();
        assert_eq!(
            names,
            vec![
                "assets".to_string(),
                "edit_scene".into(),
                "play".into(),
                "read".into()
            ]
        );
        let copied = AgentConfig {
            permissions: ["read", "edit_scene", "edit_assets", "build", "play"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            ..AgentConfig::default()
        };
        let permissions = AgentPermissions {
            config: copied,
            source: None,
        };
        assert!(!permissions.allows(Permission::Assets));
        let hint = permissions
            .denied_error(Permission::Assets)
            .data
            .and_then(|d| d["hint"].as_str().map(str::to_string))
            .unwrap_or_default();
        assert!(hint.contains("\"assets\""), "{hint}");
    }

    #[test]
    fn default_profile_grants_working_set() {
        let permissions = AgentPermissions::default();
        assert!(permissions.allows(Permission::Read));
        assert!(permissions.allows(Permission::EditScene));
        assert!(permissions.allows(Permission::Play));
        assert!(permissions.allows(Permission::Assets));
    }

    #[test]
    fn read_only_denies_everything_mutating() {
        let config = AgentConfig {
            read_only: true,
            ..AgentConfig::default()
        };
        let permissions = AgentPermissions { config, source: None };
        assert!(permissions.allows(Permission::Read));
        assert!(!permissions.allows(Permission::EditScene));
        assert!(!permissions.allows(Permission::Play));
        let error = permissions.denied_error(Permission::EditScene);
        assert_eq!(error.code, crate::protocol::PERMISSION_DENIED);
        let data = error.data.expect("carries the code");
        assert_eq!(data["code"], "F0012");
        assert!(data["hint"].as_str().unwrap_or("").contains("read_only"));
    }

    #[test]
    fn missing_permission_is_denied_with_a_fix() {
        let config = AgentConfig {
            permissions: ["read"].iter().map(|s| s.to_string()).collect(),
            ..AgentConfig::default()
        };
        let permissions = AgentPermissions { config, source: None };
        assert!(!permissions.allows(Permission::Play));
        let hint = permissions
            .denied_error(Permission::Play)
            .data
            .and_then(|d| d["hint"].as_str().map(String::from))
            .unwrap_or_default();
        assert!(hint.contains("\"play\""), "{hint}");
    }

    #[test]
    fn parses_the_heavy_call_budget() {
        let config = AgentConfig::parse("heavy_rate_limit_per_second = 5\n").expect("parses");
        assert_eq!(config.heavy_rate_limit_per_second, 5);
        assert_eq!(
            config.rate_limit_per_second,
            AgentConfig::default().rate_limit_per_second
        );
        let clamped = AgentConfig::parse("heavy_rate_limit_per_second = 0\n").expect("parses");
        assert_eq!(
            clamped.heavy_rate_limit_per_second, 1,
            "0 would refuse every heavy call"
        );
        assert_eq!(
            AgentConfig::parse("")
                .expect("parses")
                .heavy_rate_limit_per_second,
            AgentConfig::default().heavy_rate_limit_per_second
        );
        assert_eq!(
            AgentConfig::fail_closed().heavy_rate_limit_per_second,
            AgentConfig::default().heavy_rate_limit_per_second
        );
    }

    #[test]
    fn parses_agent_toml() {
        let config = AgentConfig::parse(
            "read_only = false\npermissions = [\"read\", \"play\"]\nrate_limit_per_second = 10\n",
        )
        .expect("parses");
        assert!(!config.read_only);
        assert!(config.allows_str("play"));
        assert!(!config.allows_str("edit_scene"));
        assert_eq!(config.rate_limit_per_second, 10);

        // Unknown keys and omitted keys keep defaults.
        let config = AgentConfig::parse("[extra]\nkey = 1\n").expect("parses");
        assert_eq!(config, AgentConfig::default());
        // read_only forces every permission off at the check, whatever the list says.
        let permissions = AgentPermissions {
            config: AgentConfig::parse("read_only = true\n").expect("parses"),
            source: None,
        };
        assert!(permissions.allows(Permission::Read));
        assert!(!permissions.allows(Permission::Play));
        assert!(!permissions.allows(Permission::EditScene));
    }

    #[test]
    fn audit_log_writes_json_lines() {
        let dir = std::env::temp_dir().join(format!("regia-agent-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("audit.jsonl");
        let mut log = AuditLog::open(path.clone());
        log.record(&AuditEntry {
            frame: 7,
            unix_ms: 1_700_000_000_000,
            method: "scene.spawn",
            params_hash: params_hash(&json!({ "name": "Rock" })),
            outcome: "ok",
            error_code: None,
            duration_us: 42,
            client: "test",
        });
        log.record(&AuditEntry {
            frame: 8,
            unix_ms: 1_700_000_000_001,
            method: "scene.despawn",
            params_hash: 0,
            outcome: "error",
            error_code: Some("F0003"),
            duration_us: 7,
            client: "test",
        });
        drop(log);
        let text = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"method\":\"scene.spawn\""));
        let seqs: Vec<u64> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<Value>(l).expect("json")["seq"]
                    .as_u64()
                    .expect("seq")
            })
            .collect();
        assert_eq!(seqs, vec![1, 2], "the log numbers each line");
        assert!(lines[1].contains("\"error_code\":\"F0003\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 23 § 5 names the audit line's fields exactly (no result summary, no undo id): a new
    /// or renamed field must change the spec and `book/agents/safety.md` with it (ADR-5628).
    #[test]
    fn audit_lines_carry_exactly_the_spec_fields() {
        let dir = regia_test_utils::unique_temp_dir("audit-fields");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("audit.jsonl");
        let mut log = AuditLog::open(path.clone());
        log.record(&audit_entry());
        drop(log);
        let text = std::fs::read_to_string(&path).expect("read");
        let line: Value = serde_json::from_str(text.lines().next().expect("one line")).expect("json");
        let mut keys: Vec<&str> = line
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut spec = [
            "seq",
            "frame",
            "unix_ms",
            "method",
            "params_hash",
            "outcome",
            "error_code",
            "duration_us",
            "client",
        ];
        spec.sort_unstable();
        assert_eq!(keys, spec);
        assert_eq!(line["params_hash"], "0000000000000003", "16 hex digits");
        assert!(line["error_code"].is_null(), "a success has a null error code");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 23 § 5: the default rate limit is 50 mutating calls per second, and the default
    /// profile is read, edit_scene, assets and play (ADR-5628).
    #[test]
    fn the_default_config_matches_the_spec_limits() {
        let config = AgentConfig::default();
        assert_eq!(config.rate_limit_per_second, 50);
        assert!(!config.read_only);
        let granted: Vec<&str> = config.permissions.iter().map(String::as_str).collect();
        assert_eq!(granted, ["assets", "edit_scene", "play", "read"]);
    }

    #[test]
    fn fnv1a_matches_reference_vectors() {
        // FNV-1a 64-bit reference vectors (Fowler/Noll/Vo).
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        // The params hash is over the canonical JSON text, so equal params hash equal.
        assert_eq!(params_hash(&json!({"a": 1})), params_hash(&json!({"a": 1})));
        assert_ne!(params_hash(&json!({"a": 1})), params_hash(&json!({"a": 2})));
    }

    /// The streamed hash (sprint-deep-zai-agent-protocol finding 3) is bit-identical to the
    /// string form it replaced: FNV-1a over `serde_json::to_string(params)`.
    #[test]
    fn params_hash_equals_the_hash_of_the_serialized_string() {
        // The form `params_hash` had before ADR-7454, kept verbatim as the reference.
        fn old_fnv1a(bytes: &[u8]) -> u64 {
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash
        }
        let reference =
            |params: &Value| old_fnv1a(serde_json::to_string(params).unwrap_or_default().as_bytes());
        let cases = [
            Value::Null,
            json!({}),
            json!([]),
            json!({ "name": "Rock", "count": 3, "scale": 0.25, "on": true }),
            json!({ "edits": [{ "op": "set", "path": ["a", 1, { "b": [null, -7, 1e300] }] }], "deep": { "x": { "y": { "z": [[[]]] } } } }),
            json!({ "text": "h\u{e9}llo \u{1F600} \"quoted\" back\\slash \n\t\u{1}\u{7f}", "\u{65e5}\u{672c}": "\u{2028}\u{2029}" }),
            json!({ "big": "x".repeat(1 << 20) }),
            json!({ "big_escaped": "\"\n".repeat(1 << 18), "n": u64::MAX, "i": i64::MIN }),
        ];
        for params in &cases {
            assert_eq!(
                params_hash(params),
                reference(params),
                "{}",
                serde_json::to_string(params)
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect::<String>()
            );
        }
    }
}

/// Optional host observer of dispatched calls (ADR-7841). The host owns all UI state.
/// This does not change the audit file or protocol permissions.
#[derive(Resource, Clone, Copy)]
pub struct AuditObserver {
    /// Called after the audit file opens during plugin finish.
    pub ready: fn(&mut regia_ecs::prelude::World),
    /// Called immediately before an admitted handler executes.
    pub begin: fn(&mut regia_ecs::prelude::World),
    /// Called after an audit line is recorded, including refused and expired calls.
    pub recorded: fn(&mut regia_ecs::prelude::World, &AuditEntry<'_>, &serde_json::Value),
}
