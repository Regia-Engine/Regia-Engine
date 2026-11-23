// SPDX-License-Identifier: MPL-2.0
//! The multi-process session harness (`docs/specs/41-functional-tests-and-session-harness.md` § 2.8).
//!
//! A `.session.ron` names the roles of a playthrough (one game, or a server plus clients). [`launch`]
//! starts each process with its own agent directory (`REGIA_AGENT_DIR`), waits for its agent, drives
//! the functional tests through `functest.*`, gathers verdicts and logs, and decides from verdicts and
//! logs, never from timings. The lower pieces ([`spawn_instance`], [`wait_ready`], [`log_tail`]) know
//! nothing about roles or tests so soak and smoke tools can reuse them.
//!
//! A `Remote` build starts nothing: it drives the one game instance already running on a device,
//! through the agent connection `regia install-android --attach` set up (spec 41 § 9). A role with
//! `remote` set (format v2, ADR-2137) is such a device instance inside an otherwise local session:
//! a desktop server with a device client, for example.
//!
//! Waits are counted attempts. [`Poll::interval`] is zero in tests (the loop yields instead of sleeping).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use regia_agent::protocol::Endpoint;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The output-file format version (`session-result.json`, `session-failure.json`,
/// `session-progress.json`).
pub const SESSION_FORMAT_VERSION: u32 = 1;
/// The `.session.ron` format version. v2 adds the optional role field `remote` (ADR-2137); a v1 file
/// reads unchanged. A harness older than v2 refuses a v2 file, so it never starts a device role as a
/// local process.
pub const SESSION_FILE_VERSION: u32 = 2;

/// Environment variable that points a process at its own agent directory.
pub const AGENT_DIR_ENV: &str = "REGIA_AGENT_DIR";
/// Environment variable that names the process's `SessionDriver`.
pub const DRIVER_ENV: &str = "REGIA_SESSION_DRIVER";

/// Environment variable that carries the session's `limit_frames` hint to every role (set only when
/// it is above 0).
pub const LIMIT_FRAMES_ENV: &str = "REGIA_SESSION_LIMIT_FRAMES";
/// `Remote` address that takes the URL from the project's agent endpoint, as
/// `regia install-android --attach` wrote it.
pub const REMOTE_ATTACHED: &str = "attached";

/// What a role is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoleKind {
    /// A game server. Roles start in their listed order; list servers first when clients need them.
    Server,
    /// A game client.
    Client,
    /// A single-process game.
    Game,
}

/// How the processes are made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildSpec {
    /// Build a cargo package, then run its executable.
    Cargo {
        /// Package name.
        package: String,
        /// Cargo profile (default `dev`).
        #[serde(default = "default_profile")]
        profile: String,
    },
    /// A finished executable, relative to the session file's directory or absolute.
    Packaged {
        /// Path of the executable.
        path: String,
    },
    /// One game instance already running on a device, reached through `adb` port forwarding. The
    /// harness starts and stops nothing and scans no logs. The token comes from the project's agent
    /// directory (`regia install-android --attach` writes it).
    Remote {
        /// `127.0.0.1:<port>` of the forwarded agent, or [`REMOTE_ATTACHED`]. Only the loopback
        /// address: the agent token is never sent anywhere else.
        address: String,
    },
}

fn default_profile() -> String {
    "dev".to_owned()
}

fn default_count() -> u32 {
    1
}

/// One kind of process in a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Role {
    /// Role name (`[a-z0-9_-]+`).
    pub name: String,
    /// Kind.
    pub kind: RoleKind,
    /// Instances to start.
    #[serde(default = "default_count")]
    pub count: u32,
    /// Arguments; `{server}`, `{port}` and `{param:key}` are expanded.
    #[serde(default)]
    pub args: Vec<String>,
    /// Name of a registered `SessionDriver`.
    #[serde(default)]
    pub driver: Option<String>,
    /// Whether the role is meant to render. The harness passes it to the build as
    /// `REGIA_SESSION_RENDER` (`1` or `0`); it does not choose the renderer itself. A desktop
    /// build still opens its window with `false` unless the game reads the variable or the build is
    /// a `server`-feature (headless) build.
    #[serde(default)]
    pub render: bool,
    /// Extra environment variables.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// A device instance instead of a local process (format v2): `127.0.0.1:<port>` of the forwarded
    /// agent or [`REMOTE_ATTACHED`]. The harness starts, stops and restarts nothing for it, scans
    /// no logs, and passes it no arguments (it is already running); `script` calls reach it like
    /// any role. Count 1, at most one such role per session.
    #[serde(default)]
    pub remote: Option<String>,
}

/// Which functional tests every role runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSelect {
    /// Name globs.
    #[serde(default)]
    pub filter: Vec<String>,
    /// Tags (all-of).
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One harness-driven agent call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptCall {
    /// `role` (every instance) or `role[n]`.
    pub role: String,
    /// Protocol method.
    pub call: String,
    /// Parameters.
    #[serde(default)]
    pub params: Option<Value>,
}

/// What to keep after the run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keep {
    /// Keep a screenshot on failure (rendering roles).
    #[serde(default)]
    pub screenshots_on_failure: bool,
    /// Keep logs after a clean run.
    #[serde(default)]
    pub logs: bool,
}

fn default_max_restarts() -> u32 {
    2
}

/// A parsed `.session.ron`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Format version (`1`).
    pub format_version: u32,
    /// Build.
    pub build: BuildSpec,
    /// Roles in start order.
    pub roles: Vec<Role>,
    /// Tests each role runs.
    #[serde(default)]
    pub tests: TestSelect,
    /// Harness-driven agent calls.
    #[serde(default)]
    pub script: Vec<ScriptCall>,
    /// Frame limit hint for the whole session: roles read it from `REGIA_SESSION_LIMIT_FRAMES`
    /// ([`LIMIT_FRAMES_ENV`]); the harness does not enforce it.
    #[serde(default)]
    pub limit_frames: u64,
    /// Restarts allowed per session with `--recover`.
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
    /// Log `ERROR` lines containing any of these do not fail the session.
    #[serde(default)]
    pub allow_errors: Vec<String>,
    /// What to keep.
    #[serde(default)]
    pub keep: Keep,
}

/// Why a session could not run to a verdict.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The session file is invalid.
    #[error("session file: {0}")]
    Invalid(String),
    /// File or process IO failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The cause.
        source: std::io::Error,
    },
    /// The build step failed.
    #[error("build failed: {0}")]
    Build(String),
    /// A process exited before its agent was ready.
    #[error("{instance} exited before it was ready (exit code {code:?}); log tail:\n{log_tail}")]
    ExitedBeforeReady {
        /// `role-index`.
        instance: String,
        /// Exit code, if the OS gave one.
        code: Option<i32>,
        /// Last lines of its output.
        log_tail: String,
    },
    /// A process never published its agent endpoint within the attempt budget.
    #[error("{instance} did not publish an agent endpoint within {attempts} attempts")]
    NotReady {
        /// `role-index`.
        instance: String,
        /// Attempts made.
        attempts: u32,
    },
}

fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> SessionError {
    let context = context.into();
    move |source| SessionError::Io { context, source }
}

impl Session {
    /// Parses and validates session text.
    pub fn parse(text: &str) -> Result<Self, SessionError> {
        let session: Session = ron::from_str(text).map_err(|e| SessionError::Invalid(e.to_string()))?;
        if session.format_version > SESSION_FILE_VERSION {
            return Err(SessionError::Invalid(format!(
                "format_version {} is newer than this build understands ({SESSION_FILE_VERSION})",
                session.format_version
            )));
        }
        if session.roles.is_empty() {
            return Err(SessionError::Invalid("a session needs at least one role".into()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for role in &session.roles {
            let ok = !role.name.is_empty()
                && role
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if !ok {
                return Err(SessionError::Invalid(format!(
                    "role name `{}` must match [a-z0-9_-]+",
                    role.name
                )));
            }
            if !seen.insert(role.name.clone()) {
                return Err(SessionError::Invalid(format!(
                    "role `{}` is listed twice",
                    role.name
                )));
            }
            if role.count == 0 || role.count > 64 {
                return Err(SessionError::Invalid(format!(
                    "role `{}`: count must be 1 to 64",
                    role.name
                )));
            }
        }
        session.check_remote()?;
        session.check_remote_roles()?;
        Ok(session)
    }

    /// A role-level `remote` needs format v2, a loopback address, count 1 and a local build; at most
    /// one role is remote (the token comes from the one attached device).
    fn check_remote_roles(&self) -> Result<(), SessionError> {
        let remote: Vec<&Role> = self.roles.iter().filter(|r| r.remote.is_some()).collect();
        let Some(first) = remote.first() else {
            return Ok(());
        };
        if self.format_version < 2 {
            return Err(SessionError::Invalid(format!(
                "role `{}`: `remote` needs format_version 2",
                first.name
            )));
        }
        if matches!(self.build, BuildSpec::Remote { .. }) {
            return Err(SessionError::Invalid(format!(
                "role `{}`: a Remote build is already one device instance; give roles `remote` only with a local build",
                first.name
            )));
        }
        if remote.len() > 1 {
            return Err(SessionError::Invalid(format!(
                "{} roles are remote; a session drives at most one device instance",
                remote.len()
            )));
        }
        let address = first.remote.as_deref().unwrap_or_default();
        if !remote_address_ok(address) {
            return Err(SessionError::Invalid(format!(
                "role `{}`: remote address `{address}` must be 127.0.0.1:<port> or `{REMOTE_ATTACHED}`",
                first.name
            )));
        }
        if first.count != 1 {
            return Err(SessionError::Invalid(format!(
                "role `{}`: a remote role is one device instance, count is {}",
                first.name, first.count
            )));
        }
        Ok(())
    }

    /// A `Remote` build has a loopback address and exactly one instance: a device role is one game.
    fn check_remote(&self) -> Result<(), SessionError> {
        let BuildSpec::Remote { address } = &self.build else {
            return Ok(());
        };
        if !remote_address_ok(address) {
            return Err(SessionError::Invalid(format!(
                "remote address `{address}` must be 127.0.0.1:<port> or `{REMOTE_ATTACHED}`"
            )));
        }
        let instances: u32 = self.roles.iter().map(|r| r.count).sum();
        if instances != 1 {
            return Err(SessionError::Invalid(format!(
                "a Remote build drives one game instance, this session lists {instances}"
            )));
        }
        Ok(())
    }

    /// Reads and parses a session file; errors name the file.
    pub fn load(path: &Path) -> Result<Self, SessionError> {
        let text = fs::read_to_string(path).map_err(io_err(format!("reading {}", path.display())))?;
        Self::parse(&text).map_err(|e| SessionError::Invalid(format!("{}: {e}", path.display())))
    }
}

/// The serde shape of `.session.ron` for `cargo xtask check-formats` (`docs/formats/session-ron.md`,
/// ADR-2316): the field names of [`Session`] and every struct it reaches, plus the variant names and
/// payload shapes of [`RoleKind`] and [`BuildSpec`].
#[must_use]
pub fn session_schema_fingerprint() -> String {
    use regia_core::schema::{
        join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes,
    };
    join_field_lists(&[
        serde_field_names::<Session>(),
        serde_variant_names::<BuildSpec>(),
        serde_variant_shapes::<BuildSpec>(),
        serde_field_names::<Role>(),
        serde_variant_names::<RoleKind>(),
        serde_field_names::<TestSelect>(),
        serde_field_names::<ScriptCall>(),
        serde_field_names::<Keep>(),
    ])
}

/// The `from` versions of the `.session.ron` migration steps: v1 → v2 only adds the optional role
/// field `remote` (ADR-2137), so a v1 file reads unchanged and `regia upgrade-project` rewrites only
/// its version.
#[must_use]
pub fn session_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Whether `address` is a `Remote` address the harness accepts: `127.0.0.1:<port>` (port above 0)
/// or [`REMOTE_ATTACHED`]. The agent token only goes to the loopback address.
pub fn remote_address_ok(address: &str) -> bool {
    address == REMOTE_ATTACHED
        || address
            .strip_prefix("127.0.0.1:")
            .and_then(|port| port.parse::<u16>().ok())
            .is_some_and(|port| port != 0)
}

/// One process of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    /// Role name.
    pub role: String,
    /// Index within the role.
    pub index: u32,
}

impl Instance {
    /// `role-index`, the artifact folder name.
    pub fn key(&self) -> String {
        format!("{}-{}", self.role, self.index)
    }
}

/// Bounded polling: `max_attempts` checks, `interval` apart (zero: yield instead of sleeping).
#[derive(Clone, Copy, Debug)]
pub struct Poll {
    /// Wait between attempts.
    pub interval: Duration,
    /// Attempts before giving up.
    pub max_attempts: u32,
}

impl Poll {
    /// Waits one interval (zero: yields instead of sleeping).
    pub fn pause_public(&self) {
        self.pause();
    }

    /// This poll with its attempt bound recomputed to wait `seconds` in total, keeping the
    /// cadence: the ready wait keeps [`Poll::default`] (60 s) while a caller gives a longer
    /// wait its own budget (for example `regia test --timeout-s`). A zero cadence (tests that
    /// yield instead of sleeping) bounds by attempts instead: one per second.
    #[must_use]
    pub fn with_budget(mut self, seconds: u64) -> Self {
        self.max_attempts = if self.interval.is_zero() {
            u32::try_from(seconds.max(1)).unwrap_or(u32::MAX)
        } else {
            let millis = self.interval.as_millis();
            u32::try_from(u128::from(seconds.saturating_mul(1000)) / millis.max(1)).unwrap_or(u32::MAX)
        };
        self
    }

    fn pause(&self) {
        if self.interval.is_zero() {
            std::thread::yield_now();
        } else {
            std::thread::sleep(self.interval);
        }
    }
}

impl Default for Poll {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(50),
            max_attempts: 1200,
        }
    }
}

/// A connection to a process's agent.
pub trait AgentLink {
    /// Calls one protocol method.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
}

/// Makes an [`AgentLink`] from a process's published endpoint and token.
pub type Connector = Box<dyn Fn(&Instance, &Endpoint, &str) -> Result<Box<dyn AgentLink>, String>>;

struct WsLink(regia_agent::client::Client);

impl AgentLink for WsLink {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.0.call(method, params).map_err(|e| e.to_string())
    }
}

/// The default connector: the agent's WebSocket client.
pub fn ws_connector() -> Connector {
    Box::new(|_, endpoint, token| {
        let mut client =
            regia_agent::client::Client::connect(&endpoint.url, token).map_err(|e| e.to_string())?;
        client
            .set_read_timeout(Some(Duration::from_secs(120)))
            .map_err(|e| e.to_string())?;
        Ok(Box::new(WsLink(client)) as Box<dyn AgentLink>)
    })
}

/// Options of one [`launch`].
pub struct LaunchOptions {
    /// Folder that receives `<session>/<role>-<n>/` artifacts.
    pub artifacts_dir: PathBuf,
    /// Session name (the artifact folder).
    pub session_name: String,
    /// Directory relative `Packaged` paths and cargo builds resolve against.
    pub base_dir: PathBuf,
    /// `--param key=value`.
    pub params: BTreeMap<String, String>,
    /// Write `session-progress.json`, restart dead processes and skip completed tests.
    pub recover: bool,
    /// Keep all artifacts after a clean run.
    pub keep_artifacts: bool,
    /// Seed passed to `functest.run`.
    pub seed: u64,
    /// Polling budget for waits.
    pub poll: Poll,
    /// Replaces the executable of the build (a finished build, or a fake in tests).
    pub exe_override: Option<PathBuf>,
    /// Environment for every process.
    pub env: Vec<(String, String)>,
    /// How to reach each agent; `None` is [`ws_connector`].
    pub connector: Option<Connector>,
    /// Drive `functest.*` (false: only launch, order, directories and log checks).
    pub drive_tests: bool,
}

impl LaunchOptions {
    /// Options with defaults for `session_name` under `artifacts_dir`.
    pub fn new(artifacts_dir: PathBuf, session_name: &str) -> Self {
        Self {
            artifacts_dir,
            session_name: session_name.to_owned(),
            base_dir: PathBuf::from("."),
            params: BTreeMap::new(),
            recover: false,
            keep_artifacts: false,
            seed: 0,
            poll: Poll::default(),
            exe_override: None,
            env: Vec::new(),
            connector: None,
            drive_tests: true,
        }
    }
}

/// A launched process.
pub struct Process {
    /// Which instance.
    pub instance: Instance,
    /// The child.
    pub child: Child,
    stdin: Option<ChildStdin>,
    /// The artifact folder.
    pub dir: PathBuf,
    /// The agent directory (`<dir>/agent`).
    pub agent_dir: PathBuf,
    /// Set when the harness had to stop the process.
    pub stopped_by_harness: bool,
}

/// What [`spawn_instance`] needs.
pub struct SpawnSpec<'a> {
    /// Executable.
    pub exe: &'a Path,
    /// Arguments, already expanded.
    pub args: &'a [String],
    /// Extra environment.
    pub env: &'a [(String, String)],
    /// The instance.
    pub instance: &'a Instance,
    /// Artifact folder of the instance.
    pub dir: &'a Path,
    /// Name of the instance's `SessionDriver`.
    pub driver: Option<&'a str>,
    /// Whether the instance renders.
    pub render: bool,
}

/// Starts one process with its own agent directory, stdout and stderr files, and a held-open stdin
/// (closing it asks the process to quit). No role or test knowledge.
#[tracing::instrument(skip_all, target = "regia_cli", name = "session_launch", fields(instance = %spec.instance.key()))]
pub fn spawn_instance(spec: &SpawnSpec<'_>) -> Result<Process, SessionError> {
    let agent_dir = spec.dir.join("agent");
    fs::create_dir_all(&agent_dir).map_err(io_err(format!("creating {}", agent_dir.display())))?;
    let open = |name: &str| {
        fs::File::create(spec.dir.join(name))
            .map_err(io_err(format!("creating {name} in {}", spec.dir.display())))
    };
    let mut cmd = Command::new(spec.exe);
    cmd.args(spec.args)
        .env(AGENT_DIR_ENV, &agent_dir)
        .env("REGIA_SESSION_ROLE", &spec.instance.role)
        .env("REGIA_SESSION_INDEX", spec.instance.index.to_string())
        .env("REGIA_SESSION_RENDER", if spec.render { "1" } else { "0" })
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::from(open("stdout.log")?))
        .stderr(Stdio::from(open("stderr.log")?));
    if let Some(driver) = spec.driver {
        cmd.env(DRIVER_ENV, driver);
    }
    let mut child = cmd
        .spawn()
        .map_err(io_err(format!("starting {}", spec.exe.display())))?;
    let stdin = child.stdin.take();
    Ok(Process {
        instance: spec.instance.clone(),
        child,
        stdin,
        dir: spec.dir.to_path_buf(),
        agent_dir,
        stopped_by_harness: false,
    })
}

/// Last `lines` lines of a process's stdout then stderr, for failure messages.
pub fn log_tail(dir: &Path, lines: usize) -> String {
    let mut all = Vec::new();
    for name in ["stdout.log", "stderr.log"] {
        if let Ok(text) = fs::read_to_string(dir.join(name)) {
            all.extend(text.lines().map(|l| format!("[{name}] {l}")));
        }
    }
    let skip = all.len().saturating_sub(lines);
    all[skip..].join("\n")
}

/// Waits for a process to publish its agent endpoint, within `poll`. A process that exits first is
/// [`SessionError::ExitedBeforeReady`].
pub fn wait_ready(process: &mut Process, poll: Poll) -> Result<(Endpoint, String), SessionError> {
    for _ in 0..poll.max_attempts {
        if let Ok(found) = regia_agent::endpoint::read(&process.agent_dir) {
            return Ok(found);
        }
        if let Ok(Some(status)) = process.child.try_wait() {
            // The endpoint may have been written just before the exit.
            if let Ok(found) = regia_agent::endpoint::read(&process.agent_dir) {
                return Ok(found);
            }
            return Err(SessionError::ExitedBeforeReady {
                instance: process.instance.key(),
                code: status.code(),
                log_tail: log_tail(&process.dir, 20),
            });
        }
        poll.pause();
    }
    Err(SessionError::NotReady {
        instance: process.instance.key(),
        attempts: poll.max_attempts,
    })
}

impl Process {
    /// Asks the process to quit (closes stdin), waits within `poll`, then kills it. Returns the exit code.
    pub fn stop(&mut self, poll: Poll) -> Option<i32> {
        self.stdin.take();
        for _ in 0..poll.max_attempts {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            poll.pause();
        }
        self.stopped_by_harness = true;
        let _ = self.child.kill();
        self.child.wait().ok().and_then(|s| s.code())
    }

    /// Exit code if the process has exited.
    pub fn exited(&mut self) -> Option<Option<i32>> {
        self.child.try_wait().ok().flatten().map(|s| s.code())
    }
}

/// Port assignment from the OS: bind port 0, read the number, release it.
pub fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

/// Expands `{server}`, `{port}` and `{param:key}` in one argument. Unknown `{param:..}` keys stay as
/// written so the mistake is visible in the process's own error.
pub fn expand_arg(arg: &str, server: Option<&str>, port: u16, params: &BTreeMap<String, String>) -> String {
    let mut out = arg.replace("{port}", &port.to_string());
    if let Some(server) = server {
        out = out.replace("{server}", server);
    }
    let mut from = 0;
    while let Some(found) = out[from..].find("{param:") {
        let start = from + found;
        let Some(end) = out[start..].find('}') else { break };
        let key = &out[start + 7..start + end];
        let Some(value) = params.get(key) else {
            // Leave this placeholder as written and keep scanning the rest of the argument.
            from = start + end + 1;
            continue;
        };
        let value = value.clone();
        out.replace_range(start..start + end + 1, &value);
        from = start + value.len();
    }
    out
}

/// A scan of one process's logs against the session's rules.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogFindings {
    /// `ERROR` lines not covered by `allow_errors`.
    pub errors: Vec<String>,
    /// Panic, assert or fatal marker lines (spec 28).
    pub markers: Vec<String>,
}

fn is_marker(line: &str) -> bool {
    line.contains("panicked at")
        || line.contains("assertion failed")
        || line.contains("fatal runtime error")
        || line.contains("FATAL")
}

fn is_error_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("ERROR")
        || line.contains(" ERROR ")
        || line.contains("\"level\":\"ERROR\"")
        || line.contains("\"level\": \"ERROR\"")
}

/// Scans `stdout.log`, `stderr.log` and any `*.log`/`*.jsonl` file in `dir` (not recursive).
pub fn scan_logs(dir: &Path, allow_errors: &[String]) -> LogFindings {
    let mut findings = LogFindings::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return findings;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "log" | "jsonl"))
        })
        .collect();
    files.sort();
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            if is_marker(line) {
                findings.markers.push(line.trim().to_owned());
            } else if is_error_line(line)
                && !allow_errors
                    .iter()
                    .any(|a| !a.is_empty() && line.contains(a.as_str()))
            {
                findings.errors.push(line.trim().to_owned());
            }
        }
    }
    findings
}

/// Crash bundles found in an instance folder: anything under `crash/`, and `*.dmp` / `*.minidump` files.
pub fn crash_bundles(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(dir.join("crash")) {
        found.extend(
            entries
                .flatten()
                .map(|e| format!("crash/{}", e.file_name().to_string_lossy())),
        );
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".dmp") || name.ends_with(".minidump") {
                found.push(name);
            }
        }
    }
    found.sort();
    found
}

/// `session-progress.json` (recovery state).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// Format version (`1`).
    pub format_version: u32,
    /// Completed test names per instance key.
    pub completed: BTreeMap<String, Vec<String>>,
    /// Restarts per instance key.
    pub restarts: BTreeMap<String, u32>,
}

/// Decodes standard base64 with padding (the agent's `png_base64`).
///
/// # Errors
///
/// Invalid or truncated base64.
pub fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = input.trim_end_matches('=');
    if input.is_empty() {
        return Ok(Vec::new());
    }
    if input.len() % 4 == 1 {
        return Err("truncated base64 (dangling sextet)".to_string());
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in input.as_bytes() {
        let val = ALPHABET
            .iter()
            .position(|&c| c == b)
            .ok_or_else(|| format!("invalid base64 character '{}'", b as char))? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<(), SessionError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_err(format!("creating {}", dir.display())))?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(io_err(format!("writing {}", path.display())))
}

impl Progress {
    fn save(&self, path: &Path) -> Result<(), SessionError> {
        write_json_atomic(path, &serde_json::to_value(self).unwrap_or(Value::Null))
    }

    fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| Progress {
                format_version: SESSION_FORMAT_VERSION,
                ..Progress::default()
            })
    }
}

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// Clean run.
    Passed,
    /// At least one process, test or log check failed.
    Failed,
    /// The session stopped without a decision, or a test reported `Aborted` or `Undecided`.
    Aborted,
}

/// One process of the outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessOutcome {
    /// `role-index`.
    pub instance: String,
    /// Exit code, if the OS gave one.
    pub exit_code: Option<i32>,
    /// The harness stopped it at the end.
    pub stopped_by_harness: bool,
    /// A device instance: the harness neither started nor stopped it, so `exit_code` is `None` and
    /// no logs were scanned.
    #[serde(default)]
    pub remote: bool,
    /// Restarts it needed.
    pub restarts: u32,
    /// Tests it reported: `(name, verdict, message)`.
    pub tests: Vec<(String, String, String)>,
    /// Log findings.
    pub findings: LogFindings,
    /// Crash bundles.
    pub crashes: Vec<String>,
    /// Failure screenshots tried for this process (`keep.screenshots_on_failure`), saved or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<FailureShot>,
}

/// One failure screenshot attempt (`keep.screenshots_on_failure`, spec 41 § 8): the step that failed
/// and the PNG saved next to the logs, or why there is none (the run itself is never failed by this).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureShot {
    /// The failed step: `test:<name>` or `script:<method>`.
    pub step: String,
    /// The PNG, relative to the session's artifact folder (`<instance>/failure-<n>.png`).
    pub file: Option<String>,
    /// Why there is no file (a role that does not render, no display, a failed call).
    pub reason: Option<String>,
}

/// Where and whether to save failure screenshots of one instance.
struct ShotCtx {
    enabled: bool,
    render: bool,
    key: String,
    dir: PathBuf,
}

impl ShotCtx {
    /// Saves a screenshot for the failed `step` through `seeing.screenshot`, or records why not.
    fn capture(&self, link: &mut dyn AgentLink, step: &str, shots: &mut Vec<FailureShot>) {
        if !self.enabled {
            return;
        }
        let _span = tracing::info_span!("session_failure_shot", step).entered();
        let mut shot = FailureShot {
            step: step.to_owned(),
            file: None,
            reason: None,
        };
        if !self.render {
            shot.reason = Some("the role does not render".into());
            shots.push(shot);
            return;
        }
        let name = format!("failure-{}.png", shots.len() + 1);
        match self.save(link, &name) {
            Ok(()) => shot.file = Some(format!("{}/{name}", self.key)),
            Err(reason) => shot.reason = Some(reason),
        }
        shots.push(shot);
    }

    fn save(&self, link: &mut dyn AgentLink, name: &str) -> Result<(), String> {
        let result = link
            .call("seeing.screenshot", json!({ "target": "window" }))
            .map_err(|e| format!("seeing.screenshot failed (no display?): {e}"))?;
        let b64 = result
            .get("png_base64")
            .and_then(Value::as_str)
            .ok_or("seeing.screenshot returned no png_base64")?;
        let png = decode_base64(b64)?;
        fs::create_dir_all(&self.dir).map_err(|e| format!("creating {}: {e}", self.dir.display()))?;
        let path = self.dir.join(name);
        let tmp = path.with_extension("png.tmp");
        fs::write(&tmp, &png)
            .and_then(|()| fs::rename(&tmp, &path))
            .map_err(|e| format!("writing {}: {e}", path.display()))
    }
}

/// The result of [`launch`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionOutcome {
    /// Format version (`1`).
    pub format_version: u32,
    /// Status.
    pub status: SessionStatus,
    /// Why it did not pass (empty when it did).
    pub reasons: Vec<String>,
    /// Every process.
    pub processes: Vec<ProcessOutcome>,
    /// Artifact folder of the session.
    pub artifacts: PathBuf,
}

impl SessionOutcome {
    /// Exit code of `regia test session`: 0 passed, 1 a failure, 2 aborted, 3 no tests ran.
    pub fn exit_code(&self) -> i32 {
        match self.status {
            SessionStatus::Passed => 0,
            SessionStatus::Aborted => 2,
            SessionStatus::Failed => {
                let none_ran = self.processes.iter().all(|p| p.tests.is_empty())
                    && self.reasons.iter().all(|r| r.starts_with("no_tests"));
                if none_ran { 3 } else { 1 }
            }
        }
    }
}

fn exe_for(session: &Session, opts: &LaunchOptions) -> Result<PathBuf, SessionError> {
    if let Some(exe) = &opts.exe_override {
        return Ok(exe.clone());
    }
    match &session.build {
        BuildSpec::Packaged { path } => {
            let p = Path::new(path);
            Ok(if p.is_absolute() {
                p.to_path_buf()
            } else {
                opts.base_dir.join(p)
            })
        }
        BuildSpec::Cargo { package, profile } => build_cargo(package, profile, &opts.base_dir),
        BuildSpec::Remote { address } => Err(SessionError::Invalid(format!(
            "remote build `{address}` has no executable"
        ))),
    }
}

/// Picks one distinct binary executable of the exact package from Cargo JSON output.
/// Returns `None` for no binary or ambiguous binary paths; build scripts never qualify.
pub fn executable_from_cargo_json(output: &str, package: &str) -> Option<PathBuf> {
    let matches_package = |id: &str| {
        let Some((url, fragment)) = id.rsplit_once('#') else {
            return id.split(['@', ':']).next() == Some(package);
        };
        if fragment.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            // A version-only fragment infers the name from the URL's final path component.
            let url = url.split('?').next().unwrap_or(url).trim_end_matches('/');
            return url
                .rsplit('/')
                .next()
                .map(|name| name.strip_suffix(".git").unwrap_or(name))
                == Some(package);
        }
        fragment.split(['@', ':']).next() == Some(package)
    };
    let mut paths: BTreeSet<PathBuf> = output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["reason"] == "compiler-artifact")
        .filter(|value| {
            value["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        })
        .filter(|value| value["package_id"].as_str().is_some_and(matches_package))
        .filter_map(|value| value["executable"].as_str().map(PathBuf::from))
        .collect();
    if paths.len() == 1 { paths.pop_first() } else { None }
}

fn build_cargo(package: &str, profile: &str, base: &Path) -> Result<PathBuf, SessionError> {
    let output = Command::new("cargo")
        .args([
            "build",
            "-p",
            package,
            "--profile",
            profile,
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(base)
        .stderr(Stdio::inherit())
        .output()
        .map_err(io_err("running cargo build"))?;
    if !output.status.success() {
        return Err(SessionError::Build(format!(
            "cargo build -p {package} exited with {}",
            output.status
        )));
    }
    executable_from_cargo_json(&String::from_utf8_lossy(&output.stdout), package).ok_or_else(|| {
        SessionError::Build(format!(
            "cargo built no unambiguous binary executable for package `{package}`"
        ))
    })
}

struct Running {
    instance: Instance,
    /// `None` for a remote role: the harness owns no process for it.
    process: Option<Process>,
    link: Option<Box<dyn AgentLink>>,
    restarts: u32,
    tests: Vec<(String, String, String)>,
    shots: Vec<FailureShot>,
}

// A failed capture is diagnostic only: it must not replace the original verdict or exit code.
fn capture_failure_screenshot(link: &mut Option<Box<dyn AgentLink>>, dir: &Path) {
    let mut capture = || -> Result<(), String> {
        let link = link.as_mut().ok_or_else(|| "agent unavailable".to_owned())?;
        let response = link.call("seeing.screenshot", json!({"target": "window"}))?;
        let encoded = response["png_base64"]
            .as_str()
            .ok_or_else(|| "screenshot has no png_base64".to_owned())?;
        let bytes = regia_asset_pipeline::data_uri_bytes(&format!("data:image/png;base64,{encoded}"))
            .map_err(|error| error.to_string())?;
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err("screenshot is not a PNG".into());
        }
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temp = dir.join(format!(
            "failure.{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| error.to_string())?;
        let result = (|| -> std::io::Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, dir.join("failure.png"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.map_err(|error| error.to_string())
    };
    if let Err(error) = capture() {
        tracing::warn!(target: "regia_cli", code = "session_screenshot_failed", %error, dir = %dir.display(), "cannot capture failure screenshot");
    }
}

fn target_matches(target: &str, instance: &Instance) -> bool {
    match target.split_once('[') {
        Some((role, idx)) => {
            role == instance.role && idx.trim_end_matches(']').parse::<u32>().ok() == Some(instance.index)
        }
        None => target == instance.role,
    }
}

/// Runs a session (spec 41 § 2.8) and writes `session-result.json` or `session-failure.json`.
#[tracing::instrument(skip_all, target = "regia_cli", name = "session_launch")]
pub fn launch(session: &Session, opts: &LaunchOptions) -> Result<SessionOutcome, SessionError> {
    if let BuildSpec::Remote { address } = &session.build {
        return launch_remote(session, opts, address);
    }
    session.check_remote_roles()?;
    // A session of remote roles only builds nothing.
    let exe = if session.roles.iter().any(|r| r.remote.is_none()) {
        exe_for(session, opts)?
    } else {
        PathBuf::new()
    };
    let root = opts.artifacts_dir.join(&opts.session_name);
    fs::create_dir_all(&root).map_err(io_err(format!("creating {}", root.display())))?;
    let progress_path = root.join("session-progress.json");
    let mut progress = open_progress(&progress_path, opts)?;
    let default_connector = ws_connector();
    let connector = opts.connector.as_ref().unwrap_or(&default_connector);

    // Ports: every server instance gets one from the OS; `{server}` is the first server's address.
    let mut ports: BTreeMap<String, u16> = BTreeMap::new();
    let mut server_addr: Option<String> = None;
    for role in session.roles.iter().filter(|r| r.remote.is_none()) {
        for index in 0..role.count {
            let port = free_port().map_err(io_err("assigning a port"))?;
            let key = Instance {
                role: role.name.clone(),
                index,
            }
            .key();
            if role.kind == RoleKind::Server && server_addr.is_none() {
                server_addr = Some(format!("127.0.0.1:{port}"));
            }
            ports.insert(key, port);
        }
    }

    let mut running: Vec<Running> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    let mut aborted = false;
    let spawn_one = |role: &Role, instance: &Instance| -> Result<Process, SessionError> {
        let port = ports.get(&instance.key()).copied().unwrap_or(0);
        let args: Vec<String> = role
            .args
            .iter()
            .map(|a| expand_arg(a, server_addr.as_deref(), port, &opts.params))
            .collect();
        let mut env = opts.env.clone();
        // `limit_frames` reaches every role (a role's own `env` entry still wins).
        if session.limit_frames > 0 {
            env.push((LIMIT_FRAMES_ENV.to_owned(), session.limit_frames.to_string()));
        }
        env.extend(role.env.iter().cloned());
        let dir = root.join(instance.key());
        spawn_instance(&SpawnSpec {
            exe: &exe,
            args: &args,
            env: &env,
            instance,
            dir: &dir,
            driver: role.driver.as_deref(),
            render: role.render,
        })
    };

    // Start in listed order; wait for each to be ready before the next.
    for role in &session.roles {
        for index in 0..role.count {
            let instance = Instance {
                role: role.name.clone(),
                index,
            };
            if let Some(address) = &role.remote {
                let dir = root.join(instance.key());
                let connected = fs::create_dir_all(&dir)
                    .map_err(io_err(format!("creating {}", dir.display())))
                    .and_then(|()| remote_endpoint(address, &opts.base_dir))
                    .and_then(|(endpoint, token)| {
                        connect_ready(connector, &instance, &endpoint, &token, opts.poll)
                            .map_err(|e| SessionError::Invalid(format!("{}: {e}", instance.key())))
                    });
                match connected {
                    Ok(link) => running.push(Running {
                        instance,
                        process: None,
                        link: Some(link),
                        restarts: 0,
                        tests: Vec::new(),
                        shots: Vec::new(),
                    }),
                    Err(e) => {
                        stop_all(&mut running, opts.poll);
                        return fail_launch(&root, e);
                    }
                }
                continue;
            }
            let mut process = match spawn_one(role, &instance) {
                Ok(p) => p,
                Err(e) => {
                    stop_all(&mut running, opts.poll);
                    return fail_launch(&root, e);
                }
            };
            let ready = wait_ready(&mut process, opts.poll);
            let (endpoint, token) = match ready {
                Ok(found) => found,
                Err(e) => {
                    process.stop(opts.poll);
                    stop_all(&mut running, opts.poll);
                    return fail_launch(&root, e);
                }
            };
            let link = if opts.drive_tests {
                match connect_ready(connector, &instance, &endpoint, &token, opts.poll) {
                    Ok(l) => Some(l),
                    Err(e) => {
                        process.stop(opts.poll);
                        stop_all(&mut running, opts.poll);
                        return fail_launch(&root, SessionError::Invalid(format!("{}: {e}", instance.key())));
                    }
                }
            } else {
                None
            };
            running.push(Running {
                instance,
                process: Some(process),
                link,
                restarts: 0,
                tests: Vec::new(),
                shots: Vec::new(),
            });
        }
    }

    // Drive the tests of every instance in start order.
    let mut restart_error: Option<SessionError> = None;
    if opts.drive_tests {
        let mut restarts_total: u32 = progress.restarts.values().sum();
        'instances: for slot in running.iter_mut() {
            let instance = slot.instance.clone();
            let Some(role) = session.roles.iter().find(|r| r.name == instance.role) else {
                continue;
            };
            let shot = shot_ctx(session, &root, role, &instance);
            run_script_calls(
                session,
                &instance,
                &mut slot.link,
                &mut reasons,
                &shot,
                &mut slot.shots,
            );
            // Carry the restarts already recorded by an earlier run of this session.
            slot.restarts = slot
                .restarts
                .max(progress.restarts.get(&instance.key()).copied().unwrap_or(0));
            loop {
                let run = drive_tests(
                    &mut slot.link,
                    &instance.key(),
                    &mut slot.tests,
                    session,
                    opts,
                    &mut ProgressCtx {
                        progress: &mut progress,
                        path: &progress_path,
                    },
                    &mut Shots {
                        ctx: &shot,
                        taken: &mut slot.shots,
                    },
                );
                match run {
                    Ok(()) => break,
                    Err(DriveError::NoTests) => {
                        reasons.push(format!("no_tests: {} has no test to run", instance.key()));
                        break;
                    }
                    Err(DriveError::Call(msg)) => {
                        // A remote role is never restarted: a lost connection fails the session.
                        let died = match slot.process.as_mut() {
                            Some(process) if opts.recover => wait_exit(process, opts.poll),
                            _ => None,
                        };
                        if died.is_none() {
                            reasons.push(format!("{}: {msg}", instance.key()));
                            break;
                        }
                        restarts_total += 1;
                        if restarts_total > session.max_restarts {
                            reasons.push("too_many_restarts".to_owned());
                            aborted = true;
                            break 'instances;
                        }
                        slot.restarts += 1;
                        progress.restarts.insert(instance.key(), slot.restarts);
                        if let Err(e) = progress.save(&progress_path) {
                            restart_error = Some(e);
                            break 'instances;
                        }
                        if let Some(old) = &slot.process {
                            let _ = fs::remove_file(old.agent_dir.join("endpoint.json"));
                        }
                        let mut process = match spawn_one(role, &instance) {
                            Ok(p) => p,
                            Err(e) => {
                                restart_error = Some(e);
                                break 'instances;
                            }
                        };
                        let ready = wait_ready(&mut process, opts.poll).and_then(|(endpoint, token)| {
                            connect_ready(connector, &instance, &endpoint, &token, opts.poll)
                                .map_err(|e| SessionError::Invalid(format!("{}: {e}", instance.key())))
                        });
                        match ready {
                            Ok(link) => {
                                slot.process = Some(process);
                                slot.link = Some(link);
                            }
                            Err(e) => {
                                process.stop(opts.poll);
                                restart_error = Some(e);
                                break 'instances;
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(e) = restart_error {
        // A failed restart must not leave the other roles running with no harness.
        stop_all(&mut running, opts.poll);
        return Err(e);
    }

    // Capture live rendering roles before their agent and window are stopped.
    if session.keep.screenshots_on_failure {
        let failed = aborted
            || !reasons.is_empty()
            || running.iter().any(|role| {
                role.tests.iter().any(|(_, verdict, _)| verdict != "Passed")
                    || role.process.as_ref().is_some_and(|process| {
                        let findings = scan_logs(&process.dir, &session.allow_errors);
                        !findings.markers.is_empty()
                            || !findings.errors.is_empty()
                            || !crash_bundles(&process.dir).is_empty()
                    })
            });
        if failed {
            for role in &mut running {
                if session
                    .roles
                    .iter()
                    .any(|spec| spec.name == role.instance.role && spec.render)
                {
                    capture_failure_screenshot(&mut role.link, &root.join(role.instance.key()));
                }
            }
        }
    }

    // Stop everything and decide.
    let mut processes = Vec::new();
    for r in &mut running {
        let Some(process) = r.process.as_mut() else {
            processes.push(ProcessOutcome {
                instance: r.instance.key(),
                exit_code: None,
                stopped_by_harness: false,
                remote: true,
                restarts: 0,
                tests: std::mem::take(&mut r.tests),
                findings: LogFindings::default(),
                crashes: Vec::new(),
                screenshots: std::mem::take(&mut r.shots),
            });
            continue;
        };
        let exit_code = process.stop(opts.poll);
        let findings = scan_logs(&process.dir, &session.allow_errors);
        let crashes = crash_bundles(&process.dir);
        processes.push(ProcessOutcome {
            instance: r.instance.key(),
            exit_code,
            stopped_by_harness: process.stopped_by_harness,
            remote: false,
            restarts: r.restarts,
            tests: std::mem::take(&mut r.tests),
            findings,
            crashes,
            screenshots: std::mem::take(&mut r.shots),
        });
    }
    finish_session(session, opts, &root, processes, reasons, aborted)
}

/// Runs a `Remote` session: the one instance is already running, so nothing is started, stopped or
/// restarted, and a lost connection is a failure.
fn launch_remote(
    session: &Session,
    opts: &LaunchOptions,
    address: &str,
) -> Result<SessionOutcome, SessionError> {
    session.check_remote()?;
    let Some(role) = session.roles.first() else {
        return Err(SessionError::Invalid("a session needs at least one role".into()));
    };
    let instance = Instance {
        role: role.name.clone(),
        index: 0,
    };
    let key = instance.key();
    let root = opts.artifacts_dir.join(&opts.session_name);
    let dir = root.join(&key);
    fs::create_dir_all(&dir).map_err(io_err(format!("creating {}", dir.display())))?;
    let progress_path = root.join("session-progress.json");
    let mut progress = open_progress(&progress_path, opts)?;
    let (endpoint, token) = match remote_endpoint(address, &opts.base_dir) {
        Ok(found) => found,
        Err(e) => return fail_launch(&root, e),
    };
    let default_connector = ws_connector();
    let connector = opts.connector.as_ref().unwrap_or(&default_connector);
    let mut link = match connect_ready(connector, &instance, &endpoint, &token, opts.poll) {
        Ok(link) => Some(link),
        Err(e) => return fail_launch(&root, SessionError::Invalid(format!("{key}: {e}"))),
    };
    let mut reasons = Vec::new();
    let mut tests = Vec::new();
    let mut shots = Vec::new();
    let shot = shot_ctx(session, &root, role, &instance);
    if opts.drive_tests {
        run_script_calls(session, &instance, &mut link, &mut reasons, &shot, &mut shots);
        let run = drive_tests(
            &mut link,
            &key,
            &mut tests,
            session,
            opts,
            &mut ProgressCtx {
                progress: &mut progress,
                path: &progress_path,
            },
            &mut Shots {
                ctx: &shot,
                taken: &mut shots,
            },
        );
        match run {
            Ok(()) => {}
            Err(DriveError::NoTests) => reasons.push(format!("no_tests: {key} has no test to run")),
            Err(DriveError::Call(msg)) => reasons.push(format!("{key}: {msg}")),
        }
    }
    if session.keep.screenshots_on_failure
        && role.render
        && (!reasons.is_empty() || tests.iter().any(|(_, verdict, _)| verdict != "Passed"))
    {
        capture_failure_screenshot(&mut link, &dir);
    }
    let process = ProcessOutcome {
        instance: key,
        exit_code: None,
        stopped_by_harness: false,
        remote: true,
        restarts: 0,
        tests,
        findings: LogFindings::default(),
        crashes: Vec::new(),
        screenshots: shots,
    };
    finish_session(session, opts, &root, vec![process], reasons, false)
}

/// The endpoint and token of a device instance, from the agent directory of the project that holds
/// `base_dir` (the session file's folder, or the project). A fixed `address` replaces the URL; the
/// token still comes from that directory.
pub fn remote_endpoint(address: &str, base_dir: &Path) -> Result<(Endpoint, String), SessionError> {
    let base = fs::canonicalize(base_dir).unwrap_or_else(|_| base_dir.to_path_buf());
    let project = base
        .ancestors()
        .find(|dir| regia_core::brand::is_project_root(dir))
        .unwrap_or(&base);
    let dir = regia_agent::endpoint::agent_dir(project);
    let (mut endpoint, token) = regia_agent::endpoint::read(&dir).map_err(io_err(format!(
        "reading the agent endpoint in {} (run `regia install-android --attach` first)",
        dir.display()
    )))?;
    if address != REMOTE_ATTACHED {
        endpoint.url = format!("ws://{address}");
    }
    Ok((endpoint, token))
}

/// The failure-screenshot settings of `instance` (`keep.screenshots_on_failure`, the role's `render`).
fn shot_ctx(session: &Session, root: &Path, role: &Role, instance: &Instance) -> ShotCtx {
    ShotCtx {
        enabled: session.keep.screenshots_on_failure,
        render: role.render,
        key: instance.key(),
        dir: root.join(instance.key()),
    }
}

fn open_progress(path: &Path, opts: &LaunchOptions) -> Result<Progress, SessionError> {
    if opts.recover {
        let progress = Progress::load(path);
        progress.save(path)?;
        return Ok(progress);
    }
    Ok(Progress {
        format_version: SESSION_FORMAT_VERSION,
        ..Progress::default()
    })
}

/// Harness-driven calls of one instance, before its tests.
fn run_script_calls(
    session: &Session,
    instance: &Instance,
    link: &mut Option<Box<dyn AgentLink>>,
    reasons: &mut Vec<String>,
    shot: &ShotCtx,
    shots: &mut Vec<FailureShot>,
) {
    let Some(link) = link.as_mut() else {
        return;
    };
    for call in session
        .script
        .iter()
        .filter(|c| target_matches(&c.role, instance))
    {
        if let Err(e) = link.call(&call.call, call.params.clone().unwrap_or_else(|| json!({}))) {
            reasons.push(format!(
                "{}: script call `{}` failed: {e}",
                instance.key(),
                call.call
            ));
            shot.capture(link.as_mut(), &format!("script:{}", call.call), shots);
        }
    }
}

/// Decides the session from its processes and writes the result or failure file.
fn finish_session(
    session: &Session,
    opts: &LaunchOptions,
    root: &Path,
    processes: Vec<ProcessOutcome>,
    mut reasons: Vec<String>,
    aborted: bool,
) -> Result<SessionOutcome, SessionError> {
    let aborted = aborted
        || processes
            .iter()
            .flat_map(|process| &process.tests)
            .any(|(_, verdict, _)| matches!(verdict.as_str(), "Aborted" | "Undecided"));
    for p in &processes {
        if !p.stopped_by_harness && !p.remote && p.exit_code != Some(0) {
            reasons.push(format!("{} exited with {:?}", p.instance, p.exit_code));
        }
        for (name, verdict, message) in &p.tests {
            if verdict != "Passed" {
                reasons.push(format!("{}: test `{name}` is {verdict}: {message}", p.instance));
            }
        }
        reasons.extend(
            p.findings
                .markers
                .iter()
                .map(|l| format!("{}: marker: {l}", p.instance)),
        );
        reasons.extend(
            p.findings
                .errors
                .iter()
                .map(|l| format!("{}: error line: {l}", p.instance)),
        );
        reasons.extend(
            p.crashes
                .iter()
                .map(|c| format!("{}: crash bundle {c}", p.instance)),
        );
    }
    let status = if aborted {
        SessionStatus::Aborted
    } else if reasons.is_empty() {
        SessionStatus::Passed
    } else {
        SessionStatus::Failed
    };
    let outcome = SessionOutcome {
        format_version: SESSION_FORMAT_VERSION,
        status,
        reasons,
        processes,
        artifacts: root.to_path_buf(),
    };
    let value = serde_json::to_value(&outcome).unwrap_or(Value::Null);
    let (write, remove) = if status == SessionStatus::Passed {
        ("session-result.json", "session-failure.json")
    } else {
        ("session-failure.json", "session-result.json")
    };
    let _ = fs::remove_file(root.join(remove));
    write_json_atomic(&root.join(write), &value)?;
    if status == SessionStatus::Passed && !opts.keep_artifacts && !session.keep.logs {
        for p in &outcome.processes {
            let _ = fs::remove_file(root.join(&p.instance).join("stdout.log"));
            let _ = fs::remove_file(root.join(&p.instance).join("stderr.log"));
        }
    }
    Ok(outcome)
}

/// Waits (bounded) for a process that lost its agent connection to be seen as exited.
fn wait_exit(process: &mut Process, poll: Poll) -> Option<Option<i32>> {
    // A zero interval counts attempts only: 200 yields would be over before a process could exit.
    let attempts = if poll.interval.is_zero() {
        poll.max_attempts
    } else {
        poll.max_attempts.min(200)
    };
    for _ in 0..attempts {
        if let Some(code) = process.exited() {
            return Some(code);
        }
        poll.pause();
    }
    None
}

fn fail_launch<T>(root: &Path, error: SessionError) -> Result<T, SessionError> {
    let value = json!({ "format_version": SESSION_FORMAT_VERSION, "status": "failed", "reasons": [error.to_string()] });
    let _ = write_json_atomic(&root.join("session-failure.json"), &value);
    Err(error)
}

fn stop_all(running: &mut [Running], poll: Poll) {
    for process in running.iter_mut().filter_map(|r| r.process.as_mut()) {
        process.stop(poll);
    }
}

/// Connects until the agent answers `functest.status` (its level is loaded), within `poll`.
pub fn connect_ready(
    connector: &Connector,
    instance: &Instance,
    endpoint: &Endpoint,
    token: &str,
    poll: Poll,
) -> Result<Box<dyn AgentLink>, String> {
    let mut last = String::from("never tried");
    for _ in 0..poll.max_attempts {
        match connector(instance, endpoint, token) {
            Ok(mut link) => match link.call("functest.status", json!({})) {
                Ok(_) => return Ok(link),
                Err(e) => last = e,
            },
            Err(e) => last = e,
        }
        poll.pause();
    }
    Err(format!("agent not ready: {last}"))
}

enum DriveError {
    NoTests,
    Call(String),
}

/// Failure-screenshot state handed down one run: where a capture goes and what is taken.
struct Shots<'a> {
    ctx: &'a ShotCtx,
    taken: &'a mut Vec<FailureShot>,
}

/// The recoverable run progress and the file it persists to.
struct ProgressCtx<'a> {
    progress: &'a mut Progress,
    path: &'a Path,
}

fn drive_tests(
    link: &mut Option<Box<dyn AgentLink>>,
    key: &str,
    reported: &mut Vec<(String, String, String)>,
    session: &Session,
    opts: &LaunchOptions,
    progress: &mut ProgressCtx<'_>,
    shots: &mut Shots<'_>,
) -> Result<(), DriveError> {
    let Some(link) = link.as_mut() else {
        return Ok(());
    };
    let call_err = |e: String| DriveError::Call(e);
    let listed = link
        .call(
            "functest.list",
            json!({ "filter": session.tests.filter, "tags": session.tests.tags }),
        )
        .map_err(call_err)?;
    let names: Vec<String> = listed["tests"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|t| t["enabled"] != false)
                .filter_map(|t| t["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return Err(DriveError::NoTests);
    }
    let done: Vec<String> = progress.progress.completed.get(key).cloned().unwrap_or_default();
    let remaining: Vec<&String> = names.iter().filter(|n| !done.contains(n)).collect();
    if remaining.is_empty() {
        return Ok(());
    }
    let mut params: serde_json::Map<String, Value> =
        opts.params.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    if session.limit_frames != 0 {
        params
            .entry("regia_session_limit_frames".to_owned())
            .or_insert_with(|| json!(session.limit_frames.to_string()));
    }
    link.call(
        "functest.run",
        json!({ "filter": remaining, "tags": session.tests.tags, "seed": opts.seed, "params": params }),
    )
    .map_err(call_err)?;
    let mut seen = 0usize;
    for _ in 0..opts.poll.max_attempts {
        let status = link.call("functest.status", json!({})).map_err(call_err)?;
        let results = link
            .call("functest.results", json!({ "since": seen }))
            .map_err(call_err)?;
        if let Some(tests) = results["tests"].as_array() {
            for t in tests {
                let name = t["name"].as_str().unwrap_or_default().to_owned();
                let verdict = t["verdict"].as_str().unwrap_or("Undecided").to_owned();
                let message = t["message"].as_str().unwrap_or_default().to_owned();
                seen += 1;
                if t["skipped"] != true {
                    // Only a pass is "completed": a failed or undecided test must run again on
                    // `--recover` so the failure is neither skipped nor reported as a pass.
                    if verdict == "Passed" {
                        progress
                            .progress
                            .completed
                            .entry(key.to_owned())
                            .or_default()
                            .push(name.clone());
                        if opts.recover {
                            let _ = progress.progress.save(progress.path);
                        }
                    }
                    if verdict != "Passed" {
                        shots
                            .ctx
                            .capture(link.as_mut(), &format!("test:{name}"), shots.taken);
                    }
                    reported.push((name, verdict, message));
                }
            }
        }
        if status["finished"] == true {
            return Ok(());
        }
        opts.poll.pause();
    }
    Err(DriveError::Call(format!(
        "{key}: tests did not finish within {} polls",
        opts.poll.max_attempts
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = r#"
Session(
    format_version: 1,
    build: Cargo(package: "my_game", profile: "dev"),
    roles: [
        (name: "server", kind: Server, count: 1, args: ["--level", "arena"], driver: None),
        (name: "client", kind: Client, count: 4, args: ["--connect", "{server}"], driver: Some("bot_walker")),
    ],
    tests: (filter: ["smoke*"], tags: []),
    script: [ (role: "client[0]", call: "play.wait_until", params: Some((condition: (a: 1)))) ],
    limit_frames: 36000,
    max_restarts: 2,
    allow_errors: [],
    keep: (screenshots_on_failure: true, logs: true),
)
"#;

    #[test]
    fn parses_the_spec_example() {
        let s = Session::parse(SESSION).expect("parses");
        assert_eq!(s.roles.len(), 2);
        assert_eq!(s.roles[1].count, 4);
        assert_eq!(s.roles[1].driver.as_deref(), Some("bot_walker"));
        assert_eq!(s.max_restarts, 2);
        assert_eq!(s.script[0].params.as_ref().expect("params")["condition"]["a"], 1);
        assert!(matches!(s.build, BuildSpec::Cargo { ref package, .. } if package == "my_game"));
    }

    #[test]
    fn defaults_and_validation() {
        let s = Session::parse(
            "(format_version: 1, build: Packaged(path: \"g\"), roles: [(name: \"g\", kind: Game)])",
        )
        .expect("minimal");
        assert_eq!((s.max_restarts, s.roles[0].count), (2, 1));
        for bad in [
            "(format_version: 3, build: Packaged(path: \"g\"), roles: [(name: \"g\", kind: Game)])",
            "(format_version: 1, build: Packaged(path: \"g\"), roles: [])",
            "(format_version: 1, build: Packaged(path: \"g\"), roles: [(name: \"G!\", kind: Game)])",
            "(format_version: 1, build: Packaged(path: \"g\"), roles: [(name: \"a\", kind: Game), (name: \"a\", kind: Game)])",
            "(format_version: 1, build: Packaged(path: \"g\"), roles: [(name: \"a\", kind: Game, count: 0)])",
            "nonsense",
        ] {
            assert!(Session::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn remote_builds_take_a_loopback_address_and_one_instance() {
        let session = |address: &str, count: u32| {
            Session::parse(&format!(
                "(format_version: 1, build: Remote(address: \"{address}\"), \
                 roles: [(name: \"phone\", kind: Game, count: {count})])"
            ))
        };
        for ok in ["attached", "127.0.0.1:5037", "127.0.0.1:65535"] {
            assert!(session(ok, 1).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "127.0.0.1",
            "127.0.0.1:0",
            "127.0.0.1:99999",
            "192.168.1.5:5555",
            "localhost:5555",
            "ws://127.0.0.1:5555",
            "127.0.0.1:5555/evil",
        ] {
            assert!(session(bad, 1).is_err(), "{bad}");
        }
        assert!(session("attached", 2).is_err(), "a device role is one game");
    }

    fn mixed(version: u32, roles: &str) -> Result<Session, SessionError> {
        Session::parse(&format!(
            "(format_version: {version}, build: Packaged(path: \"g\"), roles: [{roles}])"
        ))
    }

    #[test]
    fn a_role_level_remote_needs_v2_one_instance_a_loopback_address_and_a_local_build() {
        let server = "(name: \"server\", kind: Server)";
        let phone = |address: &str, count: u32| {
            format!("(name: \"phone\", kind: Client, count: {count}, remote: Some(\"{address}\"))")
        };
        let s = mixed(2, &format!("{server}, {}", phone("attached", 1))).expect("v2 mixed session");
        assert_eq!(s.roles[0].remote, None);
        assert_eq!(s.roles[1].remote.as_deref(), Some("attached"));
        assert!(mixed(2, &format!("{server}, {}", phone("127.0.0.1:5555", 1))).is_ok());
        assert!(mixed(2, &phone("attached", 1)).is_ok(), "remote roles only");
        assert!(mixed(1, server).is_ok(), "a v1 file reads unchanged");
        assert!(mixed(2, server).is_ok());
        let refused = [
            (
                1,
                format!("{server}, {}", phone("attached", 1)),
                "format_version 2",
            ),
            (2, format!("{server}, {}", phone("attached", 2)), "count"),
            (
                2,
                format!("{server}, {}", phone("192.168.1.5:5555", 1)),
                "remote address",
            ),
            (
                2,
                format!("{server}, {}", phone("127.0.0.1:0", 1)),
                "remote address",
            ),
            (
                2,
                format!(
                    "{}, (name: \"tablet\", kind: Client, remote: Some(\"attached\"))",
                    phone("attached", 1)
                ),
                "at most one",
            ),
            (3, server.to_owned(), "newer"),
        ];
        for (version, roles, why) in refused {
            let err = mixed(version, &roles).expect_err(&roles).to_string();
            assert!(err.contains(why), "{roles}: {err}");
        }
        let remote_build = Session::parse(
            "(format_version: 2, build: Remote(address: \"attached\"), \
             roles: [(name: \"phone\", kind: Game, remote: Some(\"attached\"))])",
        );
        assert!(remote_build.is_err(), "a Remote build plus a remote role");
    }

    /// The fuzz property for `.session.ron` (ADR-2137): seed-stable mutations of a v2 file never
    /// panic the parser, and every file that parses re-encodes to text that parses to the same
    /// session. Not a `fuzz.*` registry target: that registry lives in `regia_agent`, which cannot
    /// reach this crate.
    #[test]
    fn mutated_session_files_never_panic_and_parsed_ones_round_trip() {
        let fixture = "(format_version: 2, build: Cargo(package: \"my_game\", profile: \"dev\"), \
            roles: [(name: \"server\", kind: Server, args: [\"--level\", \"arena\"]), \
            (name: \"phone\", kind: Client, remote: Some(\"127.0.0.1:5555\"))], \
            tests: (filter: [\"smoke*\"], tags: []), \
            script: [(role: \"phone\", call: \"play.pause\")], max_restarts: 1)";
        let base = Session::parse(fixture).expect("fixture");
        let mut rng = regia_core::fuzz::FuzzRng::new(0x5e55_1015);
        let (mut parsed, mut refused) = (0, 0);
        for i in 0..2_000_u32 {
            let text = if i.is_multiple_of(2) {
                regia_core::fuzz::mutate_text(fixture, &mut rng)
            } else {
                regia_core::fuzz::mutate_text_values(fixture, &mut rng)
            };
            let outcome = std::panic::catch_unwind(|| Session::parse(&text));
            let Ok(result) = outcome else {
                panic!("the parser panicked on {text:?}");
            };
            match result {
                Ok(session) => {
                    parsed += 1;
                    let again = ron::to_string(&session).expect("re-encodes");
                    assert_eq!(Session::parse(&again).expect("re-parses"), session, "{text}");
                }
                Err(_) => refused += 1,
            }
        }
        assert!(parsed > 0 && refused > 0, "parsed {parsed}, refused {refused}");
        assert_eq!(
            Session::parse(&ron::to_string(&base).expect("encodes")).expect("parses"),
            base
        );
    }

    struct VerdictLink;

    impl AgentLink for VerdictLink {
        fn call(&mut self, method: &str, _params: Value) -> Result<Value, String> {
            match method {
                "functest.list" => Ok(json!({ "tests": [
                    { "name": "ok", "enabled": true }, { "name": "bad", "enabled": true } ] })),
                "functest.run" => Ok(json!({ "started": 2 })),
                "functest.status" => Ok(json!({ "finished": true })),
                "functest.results" => Ok(json!({ "tests": [
                    { "name": "ok", "verdict": "Passed", "message": "", "skipped": false },
                    { "name": "bad", "verdict": "Failed", "message": "boom", "skipped": false } ] })),
                other => Err(format!("unexpected {other}")),
            }
        }
    }

    #[test]
    fn a_failed_test_is_not_recorded_as_completed_for_recover() {
        let session = Session::parse(SESSION).expect("parses");
        let mut opts = LaunchOptions::new(PathBuf::from("."), "s");
        opts.poll = Poll {
            interval: Duration::ZERO,
            max_attempts: 4,
        };
        let mut progress = Progress::default();
        let mut reported = Vec::new();
        let mut link: Option<Box<dyn AgentLink>> = Some(Box::new(VerdictLink));
        let run = drive_tests(
            &mut link,
            "client[0]",
            &mut reported,
            &session,
            &opts,
            &mut ProgressCtx {
                progress: &mut progress,
                path: Path::new("unused-progress.json"),
            },
            &mut Shots {
                ctx: &ShotCtx {
                    enabled: false,
                    render: false,
                    key: "client[0]".into(),
                    dir: PathBuf::from("."),
                },
                taken: &mut Vec::new(),
            },
        );
        assert!(run.is_ok(), "the run drives to the end");
        assert_eq!(reported.len(), 2);
        // Only the pass is completed, so `--recover` runs `bad` again instead of skipping it.
        assert_eq!(progress.completed["client[0]"], vec!["ok".to_owned()]);
    }

    #[test]
    fn argument_expansion() {
        let params = BTreeMap::from([("map".to_owned(), "arena".to_owned())]);
        assert_eq!(
            expand_arg(
                "--connect={server}:{port}/{param:map}",
                Some("127.0.0.1:9"),
                7,
                &params
            ),
            "--connect=127.0.0.1:9:7/arena"
        );
        assert_eq!(expand_arg("{param:nope}", None, 1, &params), "{param:nope}");
        // An unknown key stays literal but later placeholders in the argument still expand.
        let port_only = BTreeMap::from([("port".to_owned(), "9000".to_owned())]);
        assert_eq!(
            expand_arg("ws://{param:host}:{param:port}/{port}", None, 7, &port_only),
            "ws://{param:host}:9000/7"
        );
    }

    /// `with_budget` recomputes the attempt bound for a wait's own time budget, keeping the
    /// cadence; the default 60 s maps to the default 1200 attempts, so `regia test`'s default
    /// status budget is unchanged (sprint-deep-zai-cli-tools finding 2).
    #[test]
    fn poll_budgets_follow_the_seconds_not_the_shared_bound() {
        let default = Poll::default();
        assert_eq!(default.interval, Duration::from_millis(50));
        assert_eq!(default.max_attempts, 1200);
        assert_eq!(
            default.with_budget(60).max_attempts,
            1200,
            "the default 60 s is the same budget"
        );
        assert_eq!(default.with_budget(300).max_attempts, 6000);
        assert_eq!(default.with_budget(1).max_attempts, 20);
        // A zero cadence (tests) bounds by attempts: one per second, never zero.
        let yielding = Poll {
            interval: Duration::ZERO,
            max_attempts: 10,
        };
        assert_eq!(yielding.with_budget(1).max_attempts, 1);
        assert_eq!(yielding.with_budget(7).max_attempts, 7);
    }

    #[test]
    fn log_rules_markers_errors_and_allow_list() {
        let dir = std::env::temp_dir().join(format!("regia-session-logs-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        fs::write(
            dir.join("stdout.log"),
            "INFO fine\nERROR texture missing\nERROR known flake\n2026 ERROR late\nthread 'main' panicked at x.rs:1\n{\"level\":\"ERROR\",\"msg\":\"j\"}\n",
        )
        .expect("write");
        let f = scan_logs(&dir, &["known flake".to_owned()]);
        assert_eq!(f.markers.len(), 1);
        assert_eq!(f.errors.len(), 3, "{:?}", f.errors);
        assert!(f.errors.iter().all(|e| !e.contains("known flake")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_executable_lookup_requires_one_exact_package_binary() {
        let artifact = |id: &str, kind: &str, path: &str| {
            json!({
                "reason": "compiler-artifact", "package_id": id,
                "target": {"kind": [kind]}, "executable": path,
            })
            .to_string()
        };
        let id = "path+file:///x/my_game#0.1.0";
        assert_eq!(
            executable_from_cargo_json(&artifact(id, "custom-build", "/build-script"), "my_game"),
            None
        );
        assert_eq!(
            executable_from_cargo_json(
                &artifact("path+file:///x/my_game/vendor/helper#0.1.0", "bin", "/helper"),
                "my_game"
            ),
            None
        );
        let ambiguous = [artifact(id, "bin", "/game"), artifact(id, "bin", "/server")].join("\n");
        assert_eq!(executable_from_cargo_json(&ambiguous, "my_game"), None);
        assert_eq!(
            executable_from_cargo_json(
                &artifact("path+file:///x/application#my_game@0.1.0", "bin", "/game"),
                "my_game"
            ),
            Some(PathBuf::from("/game"))
        );
    }

    #[test]
    fn cargo_json_executable_lookup() {
        let out = "{\"reason\":\"compiler-artifact\",\"package_id\":\"path+file:///x/other#0.1.0\",\"executable\":null}\n\
                   {\"reason\":\"compiler-artifact\",\"package_id\":\"path+file:///x/my_game#0.1.0\",\"target\":{\"kind\":[\"bin\"]},\"executable\":\"/t/debug/my_game\"}\n\
                   {\"reason\":\"build-finished\",\"success\":true}\n";
        assert_eq!(
            executable_from_cargo_json(out, "my_game"),
            Some(PathBuf::from("/t/debug/my_game"))
        );
        assert_eq!(executable_from_cargo_json(out, "absent"), None);
    }

    #[test]
    fn target_matching() {
        let i = Instance {
            role: "client".into(),
            index: 2,
        };
        assert!(target_matches("client", &i));
        assert!(target_matches("client[2]", &i));
        assert!(!target_matches("client[0]", &i));
        assert!(!target_matches("server", &i));
    }
}
