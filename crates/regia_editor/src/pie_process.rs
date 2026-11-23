// SPDX-License-Identifier: MPL-2.0
//! Play-in-editor with separate processes (`docs/specs/77-session-lifecycle.md` § 10.4,
//! `book/agents/pie-net.md`, ADR-2682).
//!
//! With the Net tab's "Separate processes" checked, starting play (or `editor.pie_net_run start`)
//! runs the session as real processes instead of in-memory clients:
//!
//! - **dedicated**: a server process and `clients` client processes, started through the same
//!   functions `regia server --clients N` uses (`regia_asset_pipeline::dev_server`);
//! - **listen**: the editor's own PIE net session hosts on a real UDP port and the `clients`
//!   client processes join it.
//!
//! The editor saves the open level to a play copy under `assets/.pie/`, writes a server directory
//! under `build/pie/` (its `server.toml` names the play copy, an ephemeral game port and an
//! ephemeral `/health` port), and passes the level and the conditioner settings to every process in
//! the environment (`REGIA_PIE_LEVEL`, `REGIA_PIE_NET_MODE`, `REGIA_NET_CONDITIONER`,
//! `REGIA_NET_CONDITIONER_SEED`). Clients start once the server printed its `listening` line, so
//! they dial the port it really bound.
//!
//! Every line a child writes goes to the Output Log with a `[server]` or `[client k]` prefix and to
//! that child's tail. Stopping play kills the children; the editor ending kills them too
//! (`regia_platform::child_bind`: the parent-death signal on Linux, a job object on Windows, best
//! effort elsewhere). A child that exits on its own is shown with its exit code and last lines.
//!
//! The binaries are the ones `regia server --clients N` keeps in `build/server-dev/bin/`
//! (`<bin>-server`, `<bin>-client`); building them from the editor is not part of this mode yet.

use std::collections::VecDeque;
use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::dev_server::{self, ClientSpec, PieExtras, ServerSpec};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::pie_net::{PieNet, PieNetMode};
use crate::viewport_agent::Handler;

/// Lines kept per child for the panel and `editor.pie_processes`.
pub const LOG_TAIL_LINES: usize = 40;
/// Editor frames between two `/health` reads of a dedicated server.
pub const HEALTH_EVERY_FRAMES: u64 = 30;
/// The Output Log target of the children's lines.
pub const LOG_TARGET: &str = "regia_editor::pie_process";

/// Which process of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProcessRole {
    /// The dedicated server.
    Server,
    /// Client `k` (0-based).
    Client(u32),
}

impl ProcessRole {
    /// The Output Log prefix: `[server]` or `[client k]`.
    #[must_use]
    pub fn prefix(self) -> String {
        match self {
            Self::Server => "[server]".to_owned(),
            Self::Client(k) => format!("[client {k}]"),
        }
    }

    /// `server` or `client`.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client(_) => "client",
        }
    }

    /// The window title of a windowed client.
    #[must_use]
    pub fn title(self) -> String {
        match self {
            Self::Server => "PIE server".to_owned(),
            Self::Client(k) => format!("PIE client {k}"),
        }
    }
}

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    /// Its exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    /// The editor stopped it (stop, editor exit).
    pub by_editor: bool,
}

impl ExitInfo {
    /// It ended on its own with a failure (or a signal it was not sent).
    #[must_use]
    pub fn crashed(&self) -> bool {
        !self.by_editor && self.code != Some(0)
    }
}

/// One child process.
#[derive(Debug)]
pub struct ProcessEntry {
    /// Server or client k.
    pub role: ProcessRole,
    /// The OS process id.
    pub pid: u32,
    /// How it ended; `None` while it runs.
    pub exit: Option<ExitInfo>,
    /// Its last lines (stdout and stderr), oldest first.
    pub tail: VecDeque<String>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

/// The session's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessState {
    /// No session.
    #[default]
    Idle,
    /// The server runs; the clients start when it printed its port.
    Starting,
    /// Server and clients started.
    Running,
    /// The session was stopped (by the editor) or its server ended.
    Stopped,
    /// The session could not start (see the error).
    Failed,
}

impl ProcessState {
    /// Stable name for the protocol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// The server and client binaries a session runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binaries {
    /// The dedicated server binary.
    pub server: PathBuf,
    /// The client binary.
    pub client: PathBuf,
}

/// A refused or failed start: `code` is the lowercase protocol code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessError {
    /// `pie_process_no_project`, `pie_process_no_level`, `pie_process_no_binaries`,
    /// `pie_process_offline`, `pie_process_spawn_failed`, `pie_process_io`; a Game tab's pop-out adds
    /// `pie_view_no_window` and `pie_view_no_process` (ADR-7831).
    pub code: &'static str,
    /// Human text.
    pub message: String,
}

fn fail<T>(code: &'static str, message: impl Into<String>) -> Result<T, ProcessError> {
    Err(ProcessError {
        code,
        message: message.into(),
    })
}

/// The `/health` poll of a dedicated server, shared with its reader thread.
#[derive(Debug, Default)]
struct HealthPoll {
    players: Mutex<Option<u32>>,
    in_flight: AtomicBool,
}

/// Play-in-editor's separate processes: configuration and the running session.
#[derive(Resource)]
pub struct PieProcesses {
    /// Binaries to run instead of the project's `build/server-dev/bin/` pair (tests, tools).
    pub binaries: Option<Binaries>,
    /// Extra environment for every child process (for example `RUST_LOG`).
    pub extra_env: Vec<(String, String)>,
    state: ProcessState,
    running_binaries: Option<Binaries>,
    mode: PieNetMode,
    clients_wanted: u32,
    clients_started: bool,
    processes: Vec<ProcessEntry>,
    lines_tx: Sender<(usize, String)>,
    /// Behind a mutex: a resource must be `Sync`, a receiver is not.
    lines_rx: Mutex<Receiver<(usize, String)>>,
    group: regia_platform::child_bind::ChildGroup,
    project: Option<PathBuf>,
    server_dir: Option<PathBuf>,
    level_copy: Option<PathBuf>,
    level_rel: Option<String>,
    port: Option<u16>,
    status_port: Option<u16>,
    addr: Option<SocketAddr>,
    join: Option<(String, String)>,
    extras: PieExtras,
    windowed: bool,
    health: Arc<HealthPoll>,
    frames: u64,
    error: Option<ProcessError>,
    was_playing: bool,
}

impl Default for PieProcesses {
    fn default() -> Self {
        let (lines_tx, lines_rx) = channel();
        Self {
            binaries: None,
            extra_env: Vec::new(),
            state: ProcessState::Idle,
            running_binaries: None,
            mode: PieNetMode::Offline,
            clients_wanted: 0,
            clients_started: false,
            processes: Vec::new(),
            lines_tx,
            lines_rx: Mutex::new(lines_rx),
            group: regia_platform::child_bind::ChildGroup::new(),
            project: None,
            server_dir: None,
            level_copy: None,
            level_rel: None,
            port: None,
            status_port: None,
            addr: None,
            join: None,
            extras: PieExtras::default(),
            windowed: false,
            health: Arc::new(HealthPoll::default()),
            frames: 0,
            error: None,
            was_playing: false,
        }
    }
}

impl Drop for PieProcesses {
    fn drop(&mut self) {
        self.kill_all();
        self.remove_files();
    }
}

/// Everything a start needs, read from the editor world.
#[derive(Debug, Clone)]
pub struct StartRequest {
    /// The project directory.
    pub project: PathBuf,
    /// The play copy's path relative to `assets/` (already written).
    pub level_rel: String,
    /// `listen` or `dedicated`.
    pub mode: PieNetMode,
    /// Client processes.
    pub clients: u32,
    /// Windowed clients ("PIE client k") instead of offscreen.
    pub windowed: bool,
    /// Conditioner preset (empty = off).
    pub preset: String,
    /// Conditioner seed.
    pub seed: u64,
}

/// Binds an ephemeral port on loopback and returns its number (the socket closes at once; the
/// server binds it right after).
fn reserve_port(udp: bool) -> std::io::Result<u16> {
    if udp {
        Ok(std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port())
    } else {
        Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
    }
}

/// The `server.toml` of a PIE server directory: loopback only, the play copy as the one level,
/// `/health` on `status_port`.
#[must_use]
pub fn server_toml(level_rel: &str, port: u16, status_port: u16, max_players: u32) -> String {
    let level = level_rel.replace('\\', "/").replace('"', "\\\"");
    format!(
        "format_version = 1\n[server]\nname = \"PIE\"\nbind = \"127.0.0.1\"\nport = {port}\nmax_players = {max_players}\n\
         [levels]\nrotation = [\"{level}\"]\n[status]\nenabled = true\nbind = \"127.0.0.1\"\nport = {status_port}\n\
         [logs]\ndir = \"logs\"\n"
    )
}

/// The `"players":N` of a `/health` body.
#[must_use]
pub fn health_players(body: &str) -> Option<u32> {
    let rest = &body[body.find("\"players\":")? + "\"players\":".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// One `GET /health` on loopback with short timeouts.
fn read_health(port: u16) -> Option<u32> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    s.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut out = String::new();
    let _ = s.take(64 * 1024).read_to_string(&mut out);
    health_players(&out)
}

/// The binaries `regia server --clients N` keeps: `build/server-dev/bin/<bin>-server` and
/// `<bin>-client` (the newest pair when several games built there).
#[must_use]
pub fn project_binaries(project: &Path) -> Option<Binaries> {
    let dir = project.join("build").join("server-dev").join("bin");
    let suffix = std::env::consts::EXE_SUFFIX;
    let mut best: Option<(std::time::SystemTime, Binaries)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(suffix).and_then(|n| n.strip_suffix("-server")) else {
            continue;
        };
        let client = dir.join(format!("{stem}-client{suffix}"));
        if !client.is_file() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| modified > *t) {
            best = Some((
                modified,
                Binaries {
                    server: entry.path(),
                    client,
                },
            ));
        }
    }
    best.map(|(_, b)| b)
}

/// The agent directory of `role` under a session's server directory.
fn agent_dir_in(server_dir: &Path, role: ProcessRole) -> PathBuf {
    let name = match role {
        ProcessRole::Server => "server".to_owned(),
        ProcessRole::Client(k) => format!("client-{k}"),
    };
    server_dir.join("agent").join(name)
}

static NEXT_SESSION: AtomicU32 = AtomicU32::new(0);

/// A name unique to this editor and session: `<pid>-<n>`.
fn session_tag() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
    )
}

fn spawn_reader(stream: impl Read + Send + 'static, index: usize, tx: Sender<(usize, String)>) {
    let spawned = std::thread::Builder::new()
        .name(format!("pie-child-{index}"))
        .spawn(move || {
            for line in std::io::BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if tx.send((index, line)).is_err() {
                    break;
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(target: "regia_editor", "PIE: cannot read a child's output: {e}");
    }
}

impl PieProcesses {
    /// A separate-process session is starting or running (the editor must not start an in-process
    /// session beside it, review rc1-rev-editor-k finding 11).
    pub(crate) fn active(&self) -> bool {
        matches!(self.state, ProcessState::Starting | ProcessState::Running)
    }

    /// The session's state.
    #[must_use]
    pub fn state(&self) -> ProcessState {
        self.state
    }

    /// The children, server first.
    #[must_use]
    pub fn processes(&self) -> &[ProcessEntry] {
        &self.processes
    }

    /// The last start error.
    #[must_use]
    pub fn error(&self) -> Option<&ProcessError> {
        self.error.as_ref()
    }

    /// The game port the clients dial.
    #[must_use]
    pub fn addr(&self) -> Option<SocketAddr> {
        self.addr
    }

    /// The agent directory child `role` of the running session advertises its endpoint in
    /// (`REGIA_AGENT_DIR`: `build/pie/<session>/agent/server` or `.../client-<k>`), for the
    /// Profiler panel's process source (ADR-9740). `None` with no session starting or running, or
    /// for the server of a listen session (the editor hosts it).
    #[must_use]
    pub fn agent_dir(&self, role: ProcessRole) -> Option<PathBuf> {
        if !matches!(self.state, ProcessState::Starting | ProcessState::Running) {
            return None;
        }
        if role == ProcessRole::Server && self.mode != PieNetMode::Dedicated {
            return None;
        }
        self.server_dir.as_deref().map(|dir| agent_dir_in(dir, role))
    }

    /// Children still running.
    #[must_use]
    pub fn alive(&self) -> usize {
        self.processes.iter().filter(|p| p.exit.is_none()).count()
    }

    /// Players joined: the dedicated server's `/health` `players`, or the players the
    /// editor-hosted listen session holds (pass that session).
    #[must_use]
    pub fn peers(&self, listen: Option<&regia_net::session::NetSession>) -> Option<u32> {
        match self.mode {
            PieNetMode::Dedicated => self.health.players.lock().ok().and_then(|p| *p),
            // The players the hosted session holds, as the server host's `/health` counts them.
            PieNetMode::Listen => listen.and_then(|s| s.server()).map(|s| s.players_in_use()),
            PieNetMode::Offline => None,
        }
    }

    /// Starts a session: the server (dedicated) and, once it printed its port, the clients. A
    /// listen session's `listen_addr` is the editor's own hosted port; the clients start at once.
    ///
    /// # Errors
    ///
    /// No binaries, a port or file could not be prepared, or the server did not start.
    pub fn start(&mut self, req: &StartRequest, listen_addr: Option<SocketAddr>) -> Result<(), ProcessError> {
        self.stop();
        self.processes.clear();
        self.error = None;
        self.state = ProcessState::Failed;
        let result = self.try_start(req, listen_addr);
        if let Err(e) = &result {
            tracing::warn!(target: "regia_editor", "PIE separate processes did not start: {}", e.message);
            self.error = Some(e.clone());
            self.kill_all();
            self.remove_files();
        }
        result
    }

    fn try_start(&mut self, req: &StartRequest, listen_addr: Option<SocketAddr>) -> Result<(), ProcessError> {
        let Some(binaries) = self.binaries.clone().or_else(|| project_binaries(&req.project)) else {
            return fail(
                "pie_process_no_binaries",
                format!(
                    "no server/client binaries in {}",
                    req.project.join("build/server-dev/bin").display()
                ),
            );
        };
        self.mode = req.mode;
        self.windowed = req.windowed;
        self.clients_wanted = req.clients;
        self.clients_started = false;
        self.project = Some(req.project.clone());
        self.level_rel = Some(req.level_rel.clone());
        self.level_copy = Some(req.project.join("assets").join(&req.level_rel));
        self.health = Arc::new(HealthPoll::default());
        self.frames = 0;
        self.join = None;
        self.extras = PieExtras {
            level: self.level_copy.as_ref().map(|p| p.display().to_string()),
            mode: req.mode.as_str().to_owned(),
            conditioner: req.preset.clone(),
            seed: req.seed,
        };
        let tag = session_tag();
        let server_dir = req.project.join("build").join("pie").join(&tag);
        self.server_dir = Some(server_dir.clone());
        match req.mode {
            PieNetMode::Offline => fail(
                "pie_process_offline",
                "the mode is offline; set listen or dedicated",
            ),
            PieNetMode::Listen => {
                let Some(addr) = listen_addr else {
                    return fail("pie_process_spawn_failed", "the listen session has no port");
                };
                self.addr = Some(dev_server::loopback_dial(addr));
                self.port = Some(addr.port());
                self.status_port = None;
                std::fs::create_dir_all(server_dir.join("content"))
                    .or_else(|e| fail("pie_process_io", format!("{}: {e}", server_dir.display())))?;
                self.state = ProcessState::Starting;
                self.start_clients(&binaries.client)?;
                // A Game tab's "Pop out" starts a client again, windowed (ADR-7831).
                self.running_binaries = Some(binaries);
                Ok(())
            }
            PieNetMode::Dedicated => {
                let port =
                    reserve_port(true).or_else(|e| fail("pie_process_io", format!("game port: {e}")))?;
                let status =
                    reserve_port(false).or_else(|e| fail("pie_process_io", format!("status port: {e}")))?;
                self.port = Some(port);
                self.status_port = Some(status);
                self.addr = None;
                let max_players = req.clients.clamp(1, crate::pie_net::MAX_PIE_CLIENTS);
                write_server_dir(
                    &req.project,
                    &server_dir,
                    &req.level_rel,
                    port,
                    status,
                    max_players,
                )?;
                let mut cmd = dev_server::server_command(&ServerSpec {
                    binary: &binaries.server,
                    project: &req.project,
                    server_dir: &server_dir,
                    level: Some(&req.level_rel),
                    port: Some(port),
                    // Its agent server answers the Profiler panel's process source (ADR-9740).
                    agent: true,
                });
                self.extras.apply(&mut cmd);
                self.spawn(ProcessRole::Server, cmd)?;
                self.state = ProcessState::Starting;
                // A client binary that is missing should fail now, not after the server is up.
                if !binaries.client.is_file() {
                    return fail(
                        "pie_process_no_binaries",
                        format!("no client binary at {}", binaries.client.display()),
                    );
                }
                self.running_binaries = Some(binaries);
                Ok(())
            }
        }
    }

    fn spawn(&mut self, role: ProcessRole, mut cmd: std::process::Command) -> Result<(), ProcessError> {
        // Each child advertises its own agent endpoint, apart from the editor's (ADR-9740).
        if let Some(dir) = &self.server_dir {
            cmd.env("REGIA_AGENT_DIR", agent_dir_in(dir, role));
        }
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        regia_platform::child_bind::bind_to_parent(&mut cmd);
        let mut child = cmd.spawn().or_else(|e| {
            fail(
                "pie_process_spawn_failed",
                format!("{} {}: {e}", role.kind(), cmd.get_program().to_string_lossy()),
            )
        })?;
        if let Err(e) = self.group.add(&child) {
            tracing::warn!(target: "regia_editor", "PIE: {} not bound to the editor's lifetime: {e}", role.prefix());
        }
        let index = self.processes.len();
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, index, self.lines_tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, index, self.lines_tx.clone());
        }
        let stdin = child.stdin.take();
        tracing::info!(target: LOG_TARGET, "{} started, pid {}", role.prefix(), child.id());
        self.processes.push(ProcessEntry {
            role,
            pid: child.id(),
            exit: None,
            tail: VecDeque::new(),
            child: Some(child),
            stdin,
        });
        Ok(())
    }

    fn start_clients(&mut self, client_bin: &Path) -> Result<(), ProcessError> {
        let (Some(project), Some(server_dir), Some(addr)) =
            (self.project.clone(), self.server_dir.clone(), self.addr)
        else {
            return Ok(());
        };
        self.clients_started = true;
        let addr = addr.to_string();
        let manifest = server_dir.join("content").join("net-manifest.ron");
        let join = self.join.clone();
        for k in 0..self.clients_wanted {
            let mut cmd = dev_server::client_command(&ClientSpec {
                binary: client_bin,
                project: &project,
                addr: &addr,
                join: join.as_ref(),
                index: k,
                manifest: &manifest,
            });
            if self.windowed {
                dev_server::make_windowed(&mut cmd, &ProcessRole::Client(k).title());
            }
            self.extras.apply(&mut cmd);
            self.spawn(ProcessRole::Client(k), cmd)?;
        }
        self.state = ProcessState::Running;
        Ok(())
    }

    /// Once per editor frame: tees the children's lines to the Output Log, starts the clients when
    /// the server printed its port, records exits and refreshes the dedicated server's `/health`.
    pub fn pump(&mut self) {
        let _span = tracing::trace_span!("pie_process_pump").entered();
        self.frames += 1;
        self.drain_lines();
        if self.state == ProcessState::Starting
            && !self.clients_started
            && self.addr.is_some()
            && let Some(client) = self.running_binaries.as_ref().map(|b| b.client.clone())
            && let Err(e) = self.start_clients(&client)
        {
            self.error = Some(e);
            self.state = ProcessState::Failed;
        }
        self.reap();
        if self.mode == PieNetMode::Dedicated
            && matches!(self.state, ProcessState::Starting | ProcessState::Running)
            && self.frames.is_multiple_of(HEALTH_EVERY_FRAMES)
        {
            self.poll_health();
        }
    }

    /// Blocks until a child writes a line or `timeout` passes, then [`pump`](Self::pump)s. Tests
    /// and tools wait on the children's output this way instead of sleeping.
    pub fn wait_and_pump(&mut self, timeout: Duration) {
        let next = self
            .lines_rx
            .lock()
            .ok()
            .and_then(|rx| rx.recv_timeout(timeout).ok());
        if let Some((index, line)) = next {
            self.take_line(index, line);
        }
        self.pump();
    }

    /// Takes every line the children wrote since the last call.
    fn drain_lines(&mut self) {
        let lines: Vec<(usize, String)> = self
            .lines_rx
            .lock()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for (index, line) in lines {
            self.take_line(index, line);
        }
    }

    /// Reads `/health` now (on this thread) and stores the player count.
    pub fn refresh_health(&mut self) {
        if let Some(port) = self.status_port
            && self.server_alive()
        {
            let players = read_health(port);
            if let Ok(mut p) = self.health.players.lock() {
                *p = players.or(*p);
            }
        }
    }

    fn server_alive(&self) -> bool {
        self.processes
            .iter()
            .any(|p| p.role == ProcessRole::Server && p.exit.is_none())
    }

    fn poll_health(&mut self) {
        let Some(port) = self.status_port else {
            return;
        };
        if self.health.in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let health = Arc::clone(&self.health);
        let spawned = std::thread::Builder::new()
            .name("pie-health".into())
            .spawn(move || {
                if let Some(players) = read_health(port)
                    && let Ok(mut p) = health.players.lock()
                {
                    *p = Some(players);
                }
                health.in_flight.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.health.in_flight.store(false, Ordering::Release);
        }
    }

    fn take_line(&mut self, index: usize, line: String) {
        let Some(entry) = self.processes.get_mut(index) else {
            return;
        };
        tracing::info!(target: LOG_TARGET, "{} {line}", entry.role.prefix());
        if entry.role == ProcessRole::Server {
            if self.addr.is_none()
                && let Some(bound) = dev_server::parse_listening_line(&line)
            {
                self.addr = Some(dev_server::loopback_dial(bound));
            }
            if self.join.is_none()
                && let Some(join) = dev_server::parse_join_line(&line)
            {
                self.join = Some(join);
            }
        }
        entry.tail.push_back(line);
        while entry.tail.len() > LOG_TAIL_LINES {
            entry.tail.pop_front();
        }
    }

    fn reap(&mut self) {
        for entry in &mut self.processes {
            if entry.exit.is_some() {
                continue;
            }
            let Some(child) = entry.child.as_mut() else {
                continue;
            };
            if let Ok(Some(status)) = child.try_wait() {
                let info = ExitInfo {
                    code: status.code(),
                    by_editor: false,
                };
                if info.crashed() {
                    tracing::error!(
                        target: LOG_TARGET,
                        "{} exited with {}; last lines: {}",
                        entry.role.prefix(),
                        info.code.map_or_else(|| "a signal".to_owned(), |c| format!("code {c}")),
                        entry.tail.iter().rev().take(5).rev().cloned().collect::<Vec<_>>().join(" | ")
                    );
                } else {
                    tracing::info!(target: LOG_TARGET, "{} exited with {status}", entry.role.prefix());
                }
                entry.exit = Some(info);
                entry.child = None;
                entry.stdin = None;
            }
        }
        if self.state == ProcessState::Running && self.mode == PieNetMode::Dedicated && !self.server_alive() {
            self.state = ProcessState::Stopped;
        }
    }

    /// Kills every child still running (marked as stopped by the editor), waits for them, and
    /// removes the play copy and the server directory. Keeps the entries for the panel.
    pub fn stop(&mut self) {
        if self.processes.is_empty() && self.state == ProcessState::Idle {
            return;
        }
        // Lines already written still reach the log and the tails.
        self.drain_lines();
        self.reap();
        self.kill_all();
        self.remove_files();
        if self.state != ProcessState::Failed {
            self.state = ProcessState::Stopped;
        }
    }

    fn kill_all(&mut self) {
        for entry in &mut self.processes {
            entry.stdin = None;
            if let Some(mut child) = entry.child.take() {
                let _ = child.kill();
                let status = child.wait().ok();
                entry.exit = Some(ExitInfo {
                    code: status.and_then(|s| s.code()),
                    by_editor: true,
                });
                tracing::info!(target: LOG_TARGET, "{} stopped by the editor", entry.role.prefix());
            }
        }
    }

    fn remove_files(&mut self) {
        if let Some(copy) = self.level_copy.take() {
            let _ = std::fs::remove_file(copy);
        }
        if let Some(dir) = self.server_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// The running entry of `role` (the newest one when it was started again).
    fn running_index(&self, role: ProcessRole) -> Option<usize> {
        self.processes
            .iter()
            .rposition(|p| p.role == role && p.exit.is_none())
    }

    /// Whether the process of `role` runs.
    #[must_use]
    pub fn process_alive(&self, role: ProcessRole) -> bool {
        self.running_index(role).is_some()
    }

    /// Stops the process of `role` alone (a Game tab's "Stop process", ADR-7831): it is killed and
    /// marked as stopped by the editor; the others keep running. Returns whether one ran.
    pub fn stop_one(&mut self, role: ProcessRole) -> bool {
        self.drain_lines();
        self.reap();
        let Some(index) = self.running_index(role) else {
            return false;
        };
        let entry = &mut self.processes[index];
        entry.stdin = None;
        if let Some(mut child) = entry.child.take() {
            let _ = child.kill();
            let status = child.wait().ok();
            entry.exit = Some(ExitInfo {
                code: status.and_then(|s| s.code()),
                by_editor: true,
            });
            tracing::info!(target: LOG_TARGET, "{} stopped by the editor (its Game tab)", role.prefix());
        }
        true
    }

    /// Starts client `k` again in its own window (a Game tab's "Pop out", ADR-7831): the offscreen
    /// process is stopped and a windowed one ("PIE client k") joins in its place. The new entry is
    /// appended; the old one stays in the list as stopped by the editor.
    ///
    /// # Errors
    /// `pie_view_no_window` for the server (it has no window), `pie_view_no_process` when the
    /// session does not run or has no such client, `pie_process_spawn_failed` when it cannot start.
    pub fn pop_out(&mut self, role: ProcessRole) -> Result<(), ProcessError> {
        let ProcessRole::Client(k) = role else {
            return fail(
                "pie_view_no_window",
                "the server process has no window to pop out",
            );
        };
        let (Some(project), Some(server_dir), Some(addr), Some(binaries)) = (
            self.project.clone(),
            self.server_dir.clone(),
            self.addr,
            self.running_binaries.clone(),
        ) else {
            return fail("pie_view_no_process", "no separate-process session runs");
        };
        if !self.active() || !self.stop_one(role) {
            return fail("pie_view_no_process", format!("{} does not run", role.prefix()));
        }
        let addr = addr.to_string();
        let manifest = server_dir.join("content").join("net-manifest.ron");
        let mut cmd = dev_server::client_command(&ClientSpec {
            binary: &binaries.client,
            project: &project,
            addr: &addr,
            join: self.join.as_ref(),
            index: k,
            manifest: &manifest,
        });
        dev_server::make_windowed(&mut cmd, &role.title());
        self.extras.apply(&mut cmd);
        self.spawn(role, cmd)
    }

    /// The `editor.pie_processes` view.
    #[must_use]
    pub fn to_json(&self, listen: Option<&regia_net::session::NetSession>, tail: usize) -> Value {
        let processes: Vec<Value> = self
            .processes
            .iter()
            .map(|p| {
                let lines: Vec<&String> = p.tail.iter().rev().take(tail).rev().collect();
                let index = match p.role {
                    ProcessRole::Client(k) => json!(k),
                    ProcessRole::Server => Value::Null,
                };
                json!({
                    "role": p.role.kind(),
                    "index": index,
                    "label": p.role.prefix(),
                    "pid": p.pid,
                    "state": if p.exit.is_some() { "exited" } else { "running" },
                    "exit_code": p.exit.and_then(|e| e.code),
                    "stopped_by_editor": p.exit.is_some_and(|e| e.by_editor),
                    "crashed": p.exit.is_some_and(|e| e.crashed()),
                    "log_tail": lines,
                })
            })
            .collect();
        json!({
            "state": self.state.as_str(),
            "mode": self.mode.as_str(),
            "windowed": self.windowed,
            "port": self.port,
            "status_port": self.status_port,
            "addr": self.addr.map(|a| a.to_string()),
            "join_code": self.join.as_ref().map(|j| j.0.clone()),
            "level": self.level_rel,
            "peers": self.peers(listen),
            "alive": self.alive(),
            "error": self.error.as_ref().map(|e| json!({ "code": e.code, "message": e.message })),
            "processes": processes,
        })
    }
}

/// Writes the PIE server directory: `server.toml`, and the project's dev manifest when
/// `regia server` wrote one.
fn write_server_dir(
    project: &Path,
    dir: &Path,
    level_rel: &str,
    port: u16,
    status_port: u16,
    max_players: u32,
) -> Result<(), ProcessError> {
    let io = |e: std::io::Error| ProcessError {
        code: "pie_process_io",
        message: format!("{}: {e}", dir.display()),
    };
    std::fs::create_dir_all(dir.join("content")).map_err(io)?;
    let toml = dir.join("server.toml");
    let tmp = dir.join("server.toml.tmp");
    std::fs::write(&tmp, server_toml(level_rel, port, status_port, max_players)).map_err(io)?;
    std::fs::rename(&tmp, &toml).map_err(io)?;
    let manifest = project
        .join("build")
        .join("server-dev")
        .join("content")
        .join("net-manifest.ron");
    if manifest.is_file() {
        std::fs::copy(&manifest, dir.join("content").join("net-manifest.ron")).map_err(io)?;
    }
    Ok(())
}

/// Saves the open level to a fresh play copy under `assets/.pie/`; returns its path relative to
/// `assets/`.
///
/// # Errors
///
/// No project, no open level, or the write failed.
pub fn write_play_copy(world: &mut World) -> Result<(PathBuf, String), ProcessError> {
    let Some(root) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
    else {
        return fail("pie_process_no_project", "no project is open");
    };
    let Some(open) = world.get_resource::<crate::level_io::OpenLevel>().cloned() else {
        return fail("pie_process_no_level", "no level is open");
    };
    let rel = format!(".pie/play-{}.level.ron", session_tag());
    let path = root.join("assets").join(&rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .or_else(|e| fail("pie_process_io", format!("{}: {e}", parent.display())))?;
    }
    crate::level_io::save_level_file(world, &open, &path)
        .or_else(|e| fail("pie_process_io", format!("play copy {}: {e}", path.display())))?;
    Ok((root, rel))
}

/// Starts the separate processes from the Net tab's configuration: writes the play copy, hosts
/// the listen session when the mode is listen, and launches the children.
///
/// # Errors
///
/// See [`ProcessError`]; the error is also kept for the panel and `editor.pie_processes`.
pub fn start(world: &mut World) -> Result<(), ProcessError> {
    let (mode, clients, preset, seed, windowed) = {
        let net = world.get_resource_or_insert_with::<PieNet>(Default::default);
        (
            net.mode,
            net.clients,
            net.preset.clone(),
            net.seed,
            net.windowed_clients,
        )
    };
    let prepared = if mode == PieNetMode::Offline {
        fail(
            "pie_process_offline",
            "the mode is offline; set listen or dedicated",
        )
    } else {
        write_play_copy(world)
    };
    let (project, level_rel) = match prepared {
        Ok(p) => p,
        Err(e) => {
            let mut procs = world.get_resource_or_insert_with::<PieProcesses>(Default::default);
            procs.stop();
            procs.state = ProcessState::Failed;
            procs.error = Some(e.clone());
            return Err(e);
        }
    };
    let listen_addr = if mode == PieNetMode::Listen {
        let mut net = world.resource_mut::<PieNet>();
        match net.start_udp_listen() {
            Ok(addr) => Some(addr),
            Err(e) => {
                let _ = std::fs::remove_file(project.join("assets").join(&level_rel));
                let err = ProcessError {
                    code: "pie_process_spawn_failed",
                    message: e.message,
                };
                let mut procs = world.get_resource_or_insert_with::<PieProcesses>(Default::default);
                procs.state = ProcessState::Failed;
                procs.error = Some(err.clone());
                return Err(err);
            }
        }
    } else {
        None
    };
    let req = StartRequest {
        project,
        level_rel,
        mode,
        clients,
        windowed,
        preset,
        seed,
    };
    let result = world
        .get_resource_or_insert_with::<PieProcesses>(Default::default)
        .start(&req, listen_addr);
    if result.is_err() && listen_addr.is_some() {
        // No client process joins a listen session that has none: stop hosting it.
        world.resource_mut::<PieNet>().stop();
    }
    result
}

/// Stops the separate processes (and the listen session hosting them).
pub fn stop(world: &mut World) {
    if let Some(mut procs) = world.get_resource_mut::<PieProcesses>() {
        procs.stop();
    }
}

/// Editor-loop hook (from `pie_net::sync_with_play`): starts the processes when play starts with
/// "separate processes" checked, stops them when play stops, and pumps them every frame.
pub fn sync_with_play(world: &mut World, playing: bool) {
    let separate = world
        .get_resource::<PieNet>()
        .is_some_and(|n| n.separate_processes && n.mode != PieNetMode::Offline);
    let was_playing = world
        .get_resource::<PieProcesses>()
        .is_some_and(|p| p.was_playing);
    if playing && !was_playing && separate {
        // The error is kept for the panel and the agent.
        let _ = start(world);
    } else if !playing && was_playing {
        stop(world);
    }
    if let Some(mut procs) = world.get_resource_mut::<PieProcesses>() {
        procs.was_playing = playing;
        procs.pump();
    } else if playing {
        world.insert_resource(PieProcesses::default());
        world.resource_mut::<PieProcesses>().was_playing = true;
    }
    // Game tabs follow a restarted session (ADR-7831).
    crate::pie_process_view::sync(world);
}

/// Draws the processes table under the Net tab's configuration.
pub fn processes_panel(ui: &mut egui::Ui, world: &mut World) {
    let Some(procs) = world.get_resource::<PieProcesses>() else {
        return;
    };
    if procs.state == ProcessState::Idle && procs.processes.is_empty() {
        return;
    }
    let listen = world.get_resource::<PieNet>().and_then(PieNet::session);
    ui.separator();
    ui.strong(format!(
        "Separate processes: {} ({}), peers {}",
        procs.state.as_str(),
        procs.mode.as_str(),
        procs
            .peers(listen)
            .map_or_else(|| "?".to_owned(), |n| n.to_string())
    ));
    if let Some(e) = &procs.error {
        ui.colored_label(egui::Color32::LIGHT_RED, format!("{}: {}", e.code, e.message));
    }
    egui::Grid::new("pie-processes").striped(true).show(ui, |ui| {
        for h in ["Process", "PID", "State", "Exit"] {
            ui.strong(h);
        }
        ui.end_row();
        for p in &procs.processes {
            ui.label(p.role.prefix());
            ui.label(p.pid.to_string());
            let state = match p.exit {
                None => "running",
                Some(e) if e.by_editor => "stopped",
                Some(e) if e.crashed() => "crashed",
                Some(_) => "exited",
            };
            ui.label(state);
            ui.label(
                p.exit
                    .and_then(|e| e.code)
                    .map_or_else(String::new, |c| c.to_string()),
            );
            ui.end_row();
        }
    });
    for p in procs
        .processes
        .iter()
        .filter(|p| p.exit.is_some_and(|e| e.crashed()))
    {
        ui.colored_label(
            egui::Color32::LIGHT_RED,
            format!(
                "{} exited with {}",
                p.role.prefix(),
                p.exit
                    .and_then(|e| e.code)
                    .map_or_else(|| "a signal".to_owned(), |c| format!("code {c}"))
            ),
        );
        for line in p.tail.iter().rev().take(8).rev() {
            ui.monospace(line);
        }
    }
    // Show a process in a Game dock tab (ADR-7831).
    crate::pie_process_view::open_buttons(ui, world);
}

/// `editor.pie_processes`.
fn pie_processes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tail = match params.get("tail") {
        None | Some(Value::Null) => 20,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`tail` must be a whole number"))?
            .min(LOG_TAIL_LINES as u64) as usize,
    };
    let mut procs = world.remove_resource::<PieProcesses>().unwrap_or_default();
    procs.pump();
    if procs.mode == PieNetMode::Dedicated {
        procs.refresh_health();
    }
    let out = {
        let net = world.get_resource::<PieNet>();
        let mut out = procs.to_json(net.and_then(PieNet::session), tail);
        if let Value::Object(map) = &mut out {
            map.insert(
                "separate_processes".into(),
                json!(net.is_some_and(|n| n.separate_processes)),
            );
        }
        out
    };
    world.insert_resource(procs);
    Ok(out)
}

/// Turns a start error into the protocol's error.
#[must_use]
pub fn rpc(e: &ProcessError) -> RpcError {
    let hint = match e.code {
        "pie_process_no_binaries" => {
            "run `regia server --project <dir> --clients 1` once: it builds build/server-dev/bin/<game>-server and -client"
        }
        "pie_process_no_project" => "open a project first",
        "pie_process_no_level" => "open or create a level first",
        "pie_process_offline" => "editor.pie_net_config { mode: \"dedicated\" } first",
        "pie_view_no_window" => "only a client has a window; the server runs headless",
        "pie_view_no_process" => "editor.pie_processes lists the processes and their state",
        _ => "see book/agents/pie-net.md",
    };
    RpcError::new(INVALID_PARAMS, e.code, e.message.clone(), hint)
}

/// The `editor.pie_processes` method.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.pie_processes",
            tool: "pie_processes",
            description: "Read play-in-editor's separate processes (Net tab, separate_processes): the session state, the game port, peers in game, and per process its pid, state, exit code and last log lines. A child that exited on its own with a failure has crashed: true.",
            params: &[ParamSchema {
                name: "tail",
                kind: "integer",
                required: false,
                description: "Log lines per process (default 20, at most 40).",
            }],
            result: "{ state, mode, windowed, port, status_port, addr, join_code, level, peers, alive, error, separate_processes, processes: [{ role, index, label, pid, state, exit_code, stopped_by_editor, crashed, log_tail }] }",
            mutating: false,
            permission: Permission::Read,
        },
        pie_processes as Handler,
    )]
    .into_iter()
    // Game dock tabs: `editor.pie_process_view` (ADR-7831).
    .chain(crate::pie_process_view::methods())
    .collect()
}

/// Test hooks shared with the other modules' panel tests of this crate.
#[cfg(test)]
pub(crate) mod test_hooks {
    use super::*;

    /// A dedicated session that advertises `server_dir`'s agent endpoint without running a child:
    /// the Profiler panel's process source needs a live server entry and `agent_dir` to resolve.
    /// What `<server_dir>/agent/server` holds decides the worker's fate (no `endpoint.json`: it
    /// retries `disconnected`; one pointing at a silent listener: it stays `connecting`).
    pub(crate) fn dedicated_session(server_dir: &Path) -> PieProcesses {
        let mut procs = PieProcesses::default();
        procs.state = ProcessState::Running;
        procs.mode = PieNetMode::Dedicated;
        procs.server_dir = Some(server_dir.to_path_buf());
        procs.processes.push(ProcessEntry {
            role: ProcessRole::Server,
            pid: 0,
            exit: None,
            tail: VecDeque::new(),
            child: None,
            stdin: None,
        });
        procs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review rc1-rev-editor-k finding 11: `separate_processes` turned off while a separate session
    /// runs (an agent's `editor.pie_net_config`) started an in-process session beside it at the next
    /// play sync. While processes run, the in-process session stays off.
    #[test]
    fn no_in_process_session_starts_beside_running_processes() {
        let mut world = World::new();
        world.insert_resource(PieNet::default());
        world
            .resource_mut::<PieNet>()
            .configure(Some(PieNetMode::Listen), Some(1), None, None)
            .expect("config");
        let mut procs = PieProcesses::default();
        procs.state = ProcessState::Running;
        procs.was_playing = true;
        world.insert_resource(procs);
        crate::pie_net::sync_with_play(&mut world, true);
        assert!(
            !world.resource::<PieNet>().running(),
            "an in-process session started beside the separate processes"
        );
        // With no separate session the same configuration starts in process, as before.
        world.resource_mut::<PieProcesses>().state = ProcessState::Stopped;
        crate::pie_net::sync_with_play(&mut world, true);
        assert!(world.resource::<PieNet>().running());
        crate::pie_net::sync_with_play(&mut world, false);
        assert!(!world.resource::<PieNet>().running());
    }

    #[test]
    fn roles_name_their_log_prefix_and_window() {
        assert_eq!(ProcessRole::Server.prefix(), "[server]");
        assert_eq!(ProcessRole::Client(2).prefix(), "[client 2]");
        assert_eq!(ProcessRole::Client(0).title(), "PIE client 0");
    }

    #[test]
    fn the_server_toml_holds_the_ports_and_the_play_copy() {
        let text = server_toml(".pie/play-1-0.level.ron", 40001, 40002, 3);
        assert!(text.contains("port = 40001"), "{text}");
        assert!(
            text.contains("rotation = [\".pie/play-1-0.level.ron\"]"),
            "{text}"
        );
        assert!(
            text.contains("[status]\nenabled = true\nbind = \"127.0.0.1\"\nport = 40002"),
            "{text}"
        );
        assert!(text.contains("max_players = 3"), "{text}");
        // The server host's own parser accepts it.
        let dir = regia_test_utils::unique_temp_dir("pie-toml");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("server.toml"), &text).expect("write");
        let resolved = regia_net::server_host::config::resolve_config(
            &dir,
            &regia_net::server_host::CliOverrides::default(),
            &|_: &str| None,
        )
        .expect("valid server.toml");
        assert_eq!(resolved.settings.port, 40001);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn health_players_reads_the_count() {
        assert_eq!(
            health_players(
                "HTTP/1.1 200 OK\r\n\r\n{\"ok\":true,\"state\":\"running\",\"tick\":5,\"players\":2,\"max_players\":4}"
            ),
            Some(2)
        );
        assert_eq!(health_players("{\"ok\":false}"), None);
    }

    #[test]
    fn reserved_ports_are_ephemeral_and_free_again() {
        let udp = reserve_port(true).expect("udp");
        let tcp = reserve_port(false).expect("tcp");
        assert_ne!(udp, 0);
        assert_ne!(tcp, 0);
        std::net::UdpSocket::bind(("127.0.0.1", udp)).expect("the reserved udp port is free again");
        std::net::TcpListener::bind(("127.0.0.1", tcp)).expect("the reserved tcp port is free again");
    }

    #[test]
    fn project_binaries_pairs_server_and_client() {
        let dir = regia_test_utils::unique_temp_dir("pie-bins");
        let bin = dir.join("build/server-dev/bin");
        std::fs::create_dir_all(&bin).expect("dir");
        assert_eq!(project_binaries(&dir), None, "empty");
        let exe = std::env::consts::EXE_SUFFIX;
        std::fs::write(bin.join(format!("arena-server{exe}")), b"x").expect("write");
        assert_eq!(project_binaries(&dir), None, "no client yet");
        std::fs::write(bin.join(format!("arena-client{exe}")), b"x").expect("write");
        let found = project_binaries(&dir).expect("pair");
        assert_eq!(found.server, bin.join(format!("arena-server{exe}")));
        assert_eq!(found.client, bin.join(format!("arena-client{exe}")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_start_without_binaries_is_refused_and_reported() {
        let dir = regia_test_utils::unique_temp_dir("pie-nobins");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let mut procs = PieProcesses::default();
        let req = StartRequest {
            project: dir.clone(),
            level_rel: ".pie/x.level.ron".into(),
            mode: PieNetMode::Dedicated,
            clients: 2,
            windowed: false,
            preset: String::new(),
            seed: 0,
        };
        let e = procs.start(&req, None).expect_err("refused");
        assert_eq!(e.code, "pie_process_no_binaries");
        assert_eq!(procs.state(), ProcessState::Failed);
        let v = procs.to_json(None, 5);
        assert_eq!(v["state"], "failed");
        assert_eq!(v["error"]["code"], "pie_process_no_binaries");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Net tab draws the processes block (state and the start error) headless, and a play
    /// start with separate processes but no project fails into that state instead of panicking.
    #[test]
    fn a_failed_start_by_play_is_shown_in_the_net_tab() {
        let mut world = World::new();
        world
            .get_resource_or_insert_with::<PieNet>(Default::default)
            .configure(Some(PieNetMode::Dedicated), Some(2), None, None)
            .expect("config");
        world.resource_mut::<PieNet>().separate_processes = true;
        crate::pie_net::sync_with_play(&mut world, true);
        let procs = world.resource::<PieProcesses>();
        assert_eq!(procs.state(), ProcessState::Failed);
        assert_eq!(procs.error().map(|e| e.code), Some("pie_process_no_project"));
        assert!(
            !world.resource::<PieNet>().running(),
            "no in-memory session starts in its place"
        );
        let ctx = egui::Context::default();
        let mut shapes = 0;
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| crate::pie_net::net_panel(ui, &mut world));
            });
            out.textures_delta.clear();
            shapes += out.shapes.len();
        }
        assert!(shapes > 0);
        crate::pie_net::sync_with_play(&mut world, false);
        assert_eq!(world.resource::<PieProcesses>().state(), ProcessState::Failed);
    }

    #[test]
    fn exits_are_crashes_only_when_the_child_failed_on_its_own() {
        let crash = ExitInfo {
            code: Some(70),
            by_editor: false,
        };
        assert!(crash.crashed());
        assert!(
            !ExitInfo {
                code: Some(0),
                by_editor: false
            }
            .crashed()
        );
        assert!(
            !ExitInfo {
                code: None,
                by_editor: true
            }
            .crashed()
        );
    }
}
