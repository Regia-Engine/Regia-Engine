// SPDX-License-Identifier: MPL-2.0
//! The tool launcher and runtime state (ADR-1523): one [`ToolHost`] resource shared by the
//! Tools panel and the `editor.tool_*` agent methods, so a person and an agent see one state.
//!
//! A launch mints a scoped token ([`regia_agent::scope::ToolTokens`]) bound to the manifest's
//! scope, starts the program with the token in its environment, and pipes its stdout and stderr
//! into a log ring. The tool talks back over the scoped listener: it takes queued work with
//! `editor.tool_poll`, answers proxied calls with `editor.tool_reply`, and posts progress and its
//! panel view. [`ToolHost::tick`] reaps exited tools, kills tools past their stop grace, hands
//! queued work to a waiting poll and expires calls; every deadline is measured from the `now`
//! the caller passes, so tests drive time with explicit instants.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia_agent::extension::DeferredReply;
use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::Permission;
use regia_agent::scope::{ScopedInbox, ToolScope, ToolTokens};
use regia_agent::server::WakeHook;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::manifest::{DiscoveredTool, ManifestSource, ToolManifest, discover, discovery_dirs};

/// The scoped listener's URL (`ws://127.0.0.1:<port>`), in the tool's environment.
pub const ENDPOINT_ENV: &str = regia_agent::scope::ENDPOINT_ENV;
/// The tool's scoped token, in its environment (never written to disk).
pub const TOKEN_ENV: &str = regia_agent::scope::TOKEN_ENV;
/// The tool's manifest id, in its environment.
pub const ID_ENV: &str = regia_agent::scope::ID_ENV;
/// The agent protocol version, in the tool's environment.
pub const PROTOCOL_ENV: &str = "REGIA_TOOL_PROTOCOL";
/// The open project's root, in the tool's environment (absent without a project).
pub const PROJECT_DIR_ENV: &str = "REGIA_PROJECT_DIR";

/// How long a proxied call waits for the tool by default.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(25);
/// The longest `timeout_ms` a proxied call may ask for.
pub const MAX_CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// The longest `timeout_ms` a tool's poll may wait.
pub const MAX_POLL_WAIT: Duration = Duration::from_secs(60);
/// How long a stopped tool has to exit after its `shutdown` event before it is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// Most tasks a tool shows; finished tasks drop oldest first.
pub const MAX_TASKS: usize = 32;
/// Most items in a tool's view.
pub const MAX_VIEW_ITEMS: usize = 256;
/// Log lines kept per tool (stdout and stderr together).
pub const LOG_LINES: usize = 200;
/// Most undelivered events per tool.
pub const MAX_QUEUED: usize = 1024;
/// Most events one poll answer carries.
pub const MAX_EVENTS_PER_POLL: usize = 64;
/// Longest text in a view item or a task, in bytes (longer text is cut).
pub const MAX_TEXT_BYTES: usize = 4096;

/// The methods every tool token may call besides its manifest scope.
pub const BASE_METHODS: &[&str] = &[
    "agent.hello",
    "agent.schema",
    "agent.guide",
    "errors.explain",
    "editor.tool_poll",
    "editor.tool_reply",
    "editor.tool_progress",
    "editor.tool_set_view",
];

/// No manifest has this id.
pub const TOOL_UNKNOWN: &str = "tool_unknown";
/// The manifest of this id did not parse or validate.
pub const TOOL_MANIFEST_INVALID: &str = "tool_manifest_invalid";
/// The tool is already running.
pub const TOOL_ALREADY_RUNNING: &str = "tool_already_running";
/// The tool is not running (or is stopping).
pub const TOOL_NOT_RUNNING: &str = "tool_not_running";
/// The program could not be started.
pub const TOOL_SPAWN_FAILED: &str = "tool_spawn_failed";
/// The tool does not provide this method.
pub const TOOL_UNKNOWN_METHOD: &str = "tool_unknown_method";
/// The tool did not answer a proxied call in time.
pub const TOOL_CALL_TIMEOUT: &str = "tool_call_timeout";
/// The tool exited before answering.
pub const TOOL_EXITED: &str = "tool_exited";
/// The tool answered a call with an error, or cannot take more work.
pub const TOOL_ERROR: &str = "tool_error";
/// A tool-side method was called without a tool token.
pub const TOOL_CALLER_REQUIRED: &str = "tool_caller_required";
/// `editor.tool_reply` names a call that is not open.
pub const TOOL_UNKNOWN_CALL: &str = "tool_unknown_call";

/// A refused tool operation: a stable code and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    /// One of the `TOOL_*` codes, `tool_token_revoked`, or `F0002` (bad parameters).
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl ToolError {
    /// A refusal with `code`.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The protocol error: the JSON-RPC class and the hint for the code.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let (class, hint) = match self.code {
            TOOL_UNKNOWN => (
                protocol::NOT_FOUND,
                "editor.tool_list lists the tools and their manifest errors",
            ),
            TOOL_MANIFEST_INVALID => (
                protocol::INVALID_PARAMS,
                "fix the .tool.toml (editor.tool_list shows the error), then editor.tool_list { refresh: true }",
            ),
            TOOL_ALREADY_RUNNING => (protocol::INVALID_REQUEST, "editor.tool_stop stops it first"),
            TOOL_NOT_RUNNING => (protocol::INVALID_REQUEST, "editor.tool_launch starts it"),
            TOOL_SPAWN_FAILED => (
                protocol::INTERNAL_ERROR,
                "check launch.program and launch.cwd in the manifest; editor.tool_status shows the log",
            ),
            TOOL_UNKNOWN_METHOD => (
                protocol::NOT_FOUND,
                "editor.tool_methods lists what the tool provides",
            ),
            TOOL_CALL_TIMEOUT => (
                protocol::TIMEOUT,
                "the tool did not answer in time; raise timeout_ms or check editor.tool_status",
            ),
            TOOL_EXITED => (
                protocol::INTERNAL_ERROR,
                "the tool exited before answering; editor.tool_status shows its exit code and log",
            ),
            TOOL_ERROR => (
                protocol::INTERNAL_ERROR,
                "the tool's own error; the message says why",
            ),
            TOOL_CALLER_REQUIRED => (
                protocol::INVALID_REQUEST,
                "only a launched tool calls this, with its REGIA_TOOL_TOKEN on REGIA_TOOL_ENDPOINT",
            ),
            TOOL_UNKNOWN_CALL => (
                protocol::NOT_FOUND,
                "the call was answered already, timed out, or belongs to another launch",
            ),
            regia_agent::scope::TOOL_TOKEN_REVOKED => (
                protocol::PERMISSION_DENIED,
                "the tool was stopped or relaunched; use the token of the current launch",
            ),
            _ => (
                protocol::INVALID_PARAMS,
                "check the method's parameters in book/agents/editor-tools.md",
            ),
        };
        RpcError::new(class, self.code, self.message.clone(), hint)
    }
}

/// Bad parameters (`F0002`).
pub(crate) fn bad_params(message: impl Into<String>) -> ToolError {
    ToolError::new("F0002", message)
}

/// Where a tool is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    /// Never launched in this editor session.
    Stopped,
    /// Its process runs.
    Running,
    /// Asked to stop; killed when its grace runs out.
    Stopping,
    /// Its process ended, with this exit code when it had one.
    Exited(Option<i32>),
    /// It could not be started.
    Failed(String),
}

impl ToolStatus {
    /// `stopped`, `running`, `stopping`, `exited` or `failed`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Exited(_) => "exited",
            Self::Failed(_) => "failed",
        }
    }

    /// Whether the process may still run (running or stopping).
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Running | Self::Stopping)
    }
}

/// The state of one task a tool reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Still going.
    Running,
    /// Finished.
    Done,
    /// Ended with an error.
    Failed,
    /// Stopped before the end.
    Cancelled,
}

impl TaskState {
    /// `running`, `done`, `failed` or `cancelled`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The state named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Running, Self::Done, Self::Failed, Self::Cancelled]
            .into_iter()
            .find(|state| state.name() == name)
    }
}

/// One task a tool reported with `editor.tool_progress`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolTask {
    /// The tool's task id.
    pub id: String,
    /// What the panel shows (default: the id).
    pub label: String,
    /// How far along, 0 to 1; `None` is indeterminate.
    pub fraction: Option<f32>,
    /// Where it is.
    pub state: TaskState,
    /// The latest status line.
    pub message: String,
    /// Update order (oldest finished tasks drop first).
    pub seq: u64,
}

impl ToolTask {
    /// The task as `editor.tool_status` lists it.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "task": self.id,
            "label": self.label,
            "fraction": self.fraction,
            "state": self.state.name(),
            "message": self.message,
        })
    }
}

/// One item of a tool's declarative panel view (`editor.tool_set_view`).
#[derive(Debug, Clone, PartialEq)]
pub enum ViewItem {
    /// A section heading.
    Heading(String),
    /// A line of text.
    Label(String),
    /// A label and a value side by side.
    Value {
        /// The label.
        label: String,
        /// The value.
        value: String,
    },
    /// A progress bar.
    Progress {
        /// The text on the bar.
        label: String,
        /// 0 to 1.
        fraction: f32,
    },
    /// A button; a press is delivered to the tool as a `click` event.
    Button {
        /// The id the click event carries.
        id: String,
        /// The text on the button.
        label: String,
    },
    /// A link the panel opens in the browser.
    Link {
        /// The link text.
        label: String,
        /// An `http` or `https` URL.
        url: String,
    },
    /// A horizontal line.
    Separator,
}

fn cut(mut text: String) -> String {
    if text.len() > MAX_TEXT_BYTES {
        let mut end = MAX_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn text_field(item: &Value, key: &str, required: bool) -> Result<String, String> {
    match item.get(key) {
        Some(Value::String(text)) => Ok(cut(text.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
        None if required => Err(format!("`{key}` is required")),
        None => Ok(String::new()),
    }
}

impl ViewItem {
    /// Parses one item: `{ kind, ... }` with `kind` = `heading`, `label`, `value`, `progress`,
    /// `button`, `link` or `separator`.
    ///
    /// # Errors
    ///
    /// What is wrong with the item.
    pub fn parse(item: &Value) -> Result<Self, String> {
        let kind = item
            .get("kind")
            .and_then(Value::as_str)
            .ok_or("`kind` is required")?;
        Ok(match kind {
            "heading" => Self::Heading(text_field(item, "text", true)?),
            "label" => Self::Label(text_field(item, "text", true)?),
            "value" => Self::Value {
                label: text_field(item, "label", true)?,
                value: text_field(item, "value", true)?,
            },
            "progress" => {
                let fraction = item
                    .get("fraction")
                    .and_then(Value::as_f64)
                    .filter(|f| f.is_finite())
                    .ok_or("`fraction` must be a finite number")?;
                Self::Progress {
                    label: text_field(item, "label", false)?,
                    fraction: fraction.clamp(0.0, 1.0) as f32,
                }
            }
            "button" => {
                let id = text_field(item, "id", true)?;
                if id.is_empty() || id.len() > super::manifest::MAX_NAME_BYTES {
                    return Err("`id` must be 1-64 bytes".into());
                }
                let label = text_field(item, "label", false)?;
                Self::Button {
                    label: if label.is_empty() { id.clone() } else { label },
                    id,
                }
            }
            "link" => {
                let url = text_field(item, "url", true)?;
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err("`url` must start with http:// or https://".into());
                }
                let label = text_field(item, "label", false)?;
                Self::Link {
                    label: if label.is_empty() { url.clone() } else { label },
                    url,
                }
            }
            "separator" => Self::Separator,
            other => {
                return Err(format!(
                    "unknown kind {other:?} (heading, label, value, progress, button, link, separator)"
                ));
            }
        })
    }

    /// The item as `editor.tool_status` lists it (the shape [`Self::parse`] reads).
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Heading(text) => json!({ "kind": "heading", "text": text }),
            Self::Label(text) => json!({ "kind": "label", "text": text }),
            Self::Value { label, value } => json!({ "kind": "value", "label": label, "value": value }),
            Self::Progress { label, fraction } => {
                json!({ "kind": "progress", "label": label, "fraction": fraction })
            }
            Self::Button { id, label } => json!({ "kind": "button", "id": id, "label": label }),
            Self::Link { label, url } => json!({ "kind": "link", "label": label, "url": url }),
            Self::Separator => json!({ "kind": "separator" }),
        }
    }
}

/// Work queued for a tool, delivered by `editor.tool_poll`.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolEvent {
    /// A proxied call: answer it with `editor.tool_reply { call }`.
    Call {
        /// The call id.
        call: u64,
        /// The provided method.
        method: String,
        /// Its parameters.
        params: Value,
    },
    /// A button of the tool's view was pressed (in the panel or by `editor.tool_click`).
    Click {
        /// The button id.
        item: String,
    },
    /// The editor asks the tool to exit (it is killed after [`STOP_GRACE`]).
    Shutdown,
}

impl ToolEvent {
    /// The event as a poll answer carries it.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Call { call, method, params } => {
                json!({ "kind": "call", "call": call, "method": method, "params": params })
            }
            Self::Click { item } => json!({ "kind": "click", "item": item }),
            Self::Shutdown => json!({ "kind": "shutdown" }),
        }
    }
}

/// A proxied call waiting for the tool's reply.
struct PendingCall {
    reply: DeferredReply,
    method: String,
    deadline: Instant,
}

/// A tool's poll waiting for work.
struct Poller {
    reply: DeferredReply,
    deadline: Instant,
}

impl Poller {
    fn answer(self, events: &[ToolEvent]) {
        let events: Vec<Value> = events.iter().map(ToolEvent::to_json).collect();
        let _ = self.reply.send(Ok(json!({ "events": events })));
    }
}

/// One launched tool: its process, token, tasks, view, log and the work in flight.
pub struct ToolRuntime {
    status: ToolStatus,
    child: Option<Child>,
    log_rx: Option<Mutex<Receiver<String>>>,
    log: VecDeque<String>,
    scope: Option<Arc<ToolScope>>,
    tasks: Vec<ToolTask>,
    task_seq: u64,
    view: Vec<ViewItem>,
    queue: VecDeque<ToolEvent>,
    poller: Option<Poller>,
    calls: BTreeMap<u64, PendingCall>,
    stop_deadline: Option<Instant>,
    requests: u64,
    launches: u32,
}

impl ToolRuntime {
    fn new(launches: u32) -> Self {
        Self {
            status: ToolStatus::Stopped,
            child: None,
            log_rx: None,
            log: VecDeque::new(),
            scope: None,
            tasks: Vec::new(),
            task_seq: 0,
            view: Vec::new(),
            queue: VecDeque::new(),
            poller: None,
            calls: BTreeMap::new(),
            stop_deadline: None,
            requests: 0,
            launches,
        }
    }

    /// The status.
    #[must_use]
    pub fn status(&self) -> &ToolStatus {
        &self.status
    }

    /// The tasks, in the order they were first reported.
    #[must_use]
    pub fn tasks(&self) -> &[ToolTask] {
        &self.tasks
    }

    /// The tool's view.
    #[must_use]
    pub fn view(&self) -> &[ViewItem] {
        &self.view
    }

    /// The last `limit` log lines, oldest first.
    #[must_use]
    pub fn log_tail(&self, limit: usize) -> Vec<String> {
        let skip = self.log.len().saturating_sub(limit);
        self.log.iter().skip(skip).cloned().collect()
    }

    /// Proxied calls waiting for the tool's reply.
    #[must_use]
    pub fn calls_in_flight(&self) -> usize {
        self.calls.len()
    }

    /// Events queued and not yet delivered.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Requests the tool has made on its current token (it connected when this is above 0).
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests
    }

    /// How many times the tool was launched in this editor session.
    #[must_use]
    pub fn launches(&self) -> u32 {
        self.launches
    }

    /// Whether a task is running or a call is in flight (the panel repaints while it is).
    #[must_use]
    pub fn is_busy(&self) -> bool {
        !self.calls.is_empty() || self.tasks.iter().any(|t| t.state == TaskState::Running)
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(cut(line));
    }

    fn drain_log(&mut self) {
        let mut lines = Vec::new();
        if let Some(rx) = self.log_rx.as_mut().and_then(|rx| rx.get_mut().ok()) {
            loop {
                match rx.try_recv() {
                    Ok(line) => lines.push(line),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.log_rx = None;
                        break;
                    }
                }
            }
        }
        for line in lines {
            self.push_log(line);
        }
    }

    /// Hands queued events to a waiting poll.
    fn deliver(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if let Some(poller) = self.poller.take() {
            let take = self.queue.len().min(MAX_EVENTS_PER_POLL);
            let events: Vec<ToolEvent> = self.queue.drain(..take).collect();
            poller.answer(&events);
        }
    }

    /// The process ended (or was never started): revoke the token, fail open calls, answer a
    /// waiting poll, drop queued work.
    fn finish(&mut self, status: ToolStatus, tokens: &ToolTokens) {
        if let Some(scope) = self.scope.take() {
            tokens.revoke(scope.id);
        }
        for (_, call) in std::mem::take(&mut self.calls) {
            let error = ToolError::new(
                TOOL_EXITED,
                format!("the tool exited before answering {}", call.method),
            );
            let _ = call.reply.send(Err(error.rpc()));
        }
        if let Some(poller) = self.poller.take() {
            poller.answer(&[]);
        }
        self.queue.clear();
        self.child = None;
        self.stop_deadline = None;
        self.status = status;
    }

    fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }
}

/// What the launcher passes to the process it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// The resolved program.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Its working directory.
    pub cwd: PathBuf,
    /// The manifest's variables plus the launcher's `REGIA_TOOL_*` and `REGIA_PROJECT_DIR`.
    pub env: Vec<(String, String)>,
}

impl SpawnRequest {
    /// The request for `manifest` (found in `dir`) with the scoped `token` on `endpoint`.
    #[must_use]
    pub fn new(
        manifest: &ToolManifest,
        dir: &Path,
        endpoint: SocketAddr,
        token: &str,
        project: Option<&Path>,
    ) -> Self {
        let mut env: Vec<(String, String)> = manifest
            .launch
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        env.push((ENDPOINT_ENV.into(), format!("ws://{endpoint}")));
        env.push((TOKEN_ENV.into(), token.into()));
        env.push((ID_ENV.into(), manifest.id.clone()));
        env.push((PROTOCOL_ENV.into(), protocol::PROTOCOL_VERSION.into()));
        if let Some(project) = project {
            env.push((PROJECT_DIR_ENV.into(), project.display().to_string()));
        }
        Self {
            program: super::manifest::resolve_program(dir, &manifest.launch.program),
            args: manifest.launch.args.clone(),
            cwd: super::manifest::resolve_cwd(dir, manifest.launch.cwd.as_deref()),
            env,
        }
    }

    /// Starts the process with piped stdout and stderr and no stdin.
    ///
    /// # Errors
    ///
    /// The OS error when the program cannot be started.
    pub fn spawn(&self) -> std::io::Result<Child> {
        Command::new(&self.program)
            .args(&self.args)
            .current_dir(&self.cwd)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
}

/// How a tool's process ended, for the editor's log: the exit code, or the signal that killed it.
fn describe_exit(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "terminated without an exit code".to_string()
}

fn pump(reader: impl Read, tx: &Sender<String>, prefix: &str) {
    for_each_line_lossy(reader, MAX_LOG_LINE, |line| {
        tx.send(format!("{prefix}{line}")).is_ok()
    });
}

/// The longest log line kept from a child pipe; the rest of a longer line is read and dropped.
pub(crate) const MAX_LOG_LINE: usize = 64 * 1024;

/// Reads `reader` line by line until end of file or until `each` returns `false`.
///
/// Bytes, not `BufRead::lines`: a non-UTF-8 byte is replaced (`from_utf8_lossy`) instead of
/// ending the read, which closed the child's pipe and killed it with `SIGPIPE` on its next write
/// (audit 11-03 editor M6). A line longer than `max_line` bytes is cut there; its tail is still
/// read, so the child never blocks on a full pipe. A read error ends the loop.
pub(crate) fn for_each_line_lossy(reader: impl Read, max_line: usize, mut each: impl FnMut(String) -> bool) {
    let mut reader = BufReader::new(reader);
    let mut line: Vec<u8> = Vec::new();
    loop {
        let (consumed, ended) = {
            let Ok(buf) = reader.fill_buf() else {
                return;
            };
            if buf.is_empty() {
                break;
            }
            match buf.iter().position(|&byte| byte == b'\n') {
                Some(at) => {
                    let room = max_line.saturating_sub(line.len());
                    line.extend_from_slice(&buf[..at.min(room)]);
                    (at + 1, true)
                }
                None => {
                    let room = max_line.saturating_sub(line.len());
                    line.extend_from_slice(&buf[..buf.len().min(room)]);
                    (buf.len(), false)
                }
            }
        };
        reader.consume(consumed);
        if ended {
            let text = String::from_utf8_lossy(&line).trim_end_matches('\r').to_string();
            line.clear();
            if !each(text) {
                return;
            }
        }
    }
    if !line.is_empty() {
        let text = String::from_utf8_lossy(&line).trim_end_matches('\r').to_string();
        let _ = each(text);
    }
}

/// Reads a child's pipes line by line into one channel (two reader threads).
fn pipe_log(child: &mut Child, id: &str) -> Mutex<Receiver<String>> {
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("tool-{id}-stdout"))
            .spawn(move || pump(stdout, &tx, ""));
        if let Err(err) = spawned {
            tracing::warn!(target: "regia_editor", "tool {id}: cannot read stdout: {err}");
        }
    }
    if let Some(stderr) = child.stderr.take() {
        let spawned = std::thread::Builder::new()
            .name(format!("tool-{id}-stderr"))
            .spawn(move || pump(stderr, &tx, "stderr: "));
        if let Err(err) = spawned {
            tracing::warn!(target: "regia_editor", "tool {id}: cannot read stderr: {err}");
        }
    }
    Mutex::new(rx)
}

/// The editor's tools: what was discovered, what runs, and the scoped listener.
#[derive(Resource, Default)]
pub struct ToolHost {
    discovered: Vec<DiscoveredTool>,
    discovered_for: Option<Option<PathBuf>>,
    runtimes: BTreeMap<String, ToolRuntime>,
    tokens: ToolTokens,
    endpoint: Option<SocketAddr>,
    next_call: u64,
    autostarted_for: Option<PathBuf>,
    /// The tool the panel shows.
    pub selected: Option<String>,
    /// Set by `editor.tool_panel`; the shell focuses the Tools tab and clears it.
    pub focus_panel: bool,
    /// The last launch or stop error, shown in the panel until the next action.
    pub last_error: Option<String>,
}

impl ToolHost {
    /// An empty host (nothing discovered, nothing running).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-reads the manifests for `project` from the standard directories.
    pub fn refresh(&mut self, project: Option<&Path>) {
        self.refresh_from(&discovery_dirs(project));
        self.discovered_for = Some(project.map(Path::to_path_buf));
    }

    /// Re-reads the manifests from `dirs` (tests and hosts with their own directories).
    pub fn refresh_from(&mut self, dirs: &[(PathBuf, ManifestSource)]) {
        self.discovered = discover(dirs);
        self.discovered_for = None;
    }

    /// Reads the manifests for `project` unless they were read for it already. Returns whether
    /// it read them.
    pub fn ensure_discovered(&mut self, project: Option<&Path>) -> bool {
        let wanted = Some(project.map(Path::to_path_buf));
        if self.discovered_for == wanted {
            return false;
        }
        self.refresh(project);
        true
    }

    /// Every manifest file found, valid or not.
    #[must_use]
    pub fn discovered(&self) -> &[DiscoveredTool] {
        &self.discovered
    }

    /// The manifest entry of `id`: the valid one when there is one.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&DiscoveredTool> {
        self.discovered
            .iter()
            .find(|tool| tool.manifest.as_ref().is_ok_and(|m| m.id == id))
            .or_else(|| self.discovered.iter().find(|tool| tool.id() == id))
    }

    /// The valid manifest of `id`.
    ///
    /// # Errors
    ///
    /// [`TOOL_UNKNOWN`] or [`TOOL_MANIFEST_INVALID`].
    pub fn manifest(&self, id: &str) -> Result<&ToolManifest, ToolError> {
        match self.find(id) {
            None => Err(ToolError::new(
                TOOL_UNKNOWN,
                format!("no tool manifest has id `{id}`"),
            )),
            Some(tool) => tool.manifest.as_ref().map_err(|error| {
                ToolError::new(TOOL_MANIFEST_INVALID, format!("{}: {error}", tool.path.display()))
            }),
        }
    }

    /// The runtime state of `id`, once it was launched.
    #[must_use]
    pub fn runtime(&self, id: &str) -> Option<&ToolRuntime> {
        self.runtimes.get(id)
    }

    /// The status of `id` (`Stopped` before its first launch).
    #[must_use]
    pub fn status(&self, id: &str) -> ToolStatus {
        self.runtimes
            .get(id)
            .map_or(ToolStatus::Stopped, |runtime| runtime.status.clone())
    }

    /// Whether any tool runs a task or has a call in flight.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.runtimes.values().any(ToolRuntime::is_busy)
    }

    /// The live scoped tokens (one per running tool).
    #[must_use]
    pub fn tokens(&self) -> &ToolTokens {
        &self.tokens
    }

    /// The scoped listener's address, once started.
    #[must_use]
    pub fn endpoint(&self) -> Option<SocketAddr> {
        self.endpoint
    }

    /// Starts the scoped listener unless it runs. Returns the inbox the caller must insert into
    /// the world (once, at the start).
    ///
    /// # Errors
    ///
    /// [`TOOL_SPAWN_FAILED`] when the listener cannot bind.
    pub fn ensure_listener(&mut self, wake: Option<WakeHook>) -> Result<Option<ScopedInbox>, ToolError> {
        if self.endpoint.is_some() {
            return Ok(None);
        }
        let server = regia_agent::server::start_scoped(self.tokens.clone(), wake).map_err(|err| {
            ToolError::new(
                TOOL_SPAWN_FAILED,
                format!("cannot start the tool listener: {err}"),
            )
        })?;
        self.endpoint = Some(server.addr);
        Ok(Some(ScopedInbox {
            inbox: server.inbox,
            tokens: self.tokens.clone(),
        }))
    }

    /// The scope a launch of `manifest` gets: its own plus [`BASE_METHODS`] and `read`.
    #[must_use]
    pub fn scope_for(manifest: &ToolManifest) -> ToolScope {
        let mut permissions = manifest.scope_permissions();
        if !permissions.contains(&Permission::Read) {
            permissions.push(Permission::Read);
        }
        let mut methods = manifest.scope.methods.clone();
        methods.extend(BASE_METHODS.iter().map(|m| (*m).to_string()));
        ToolScope::new(manifest.id.clone(), permissions, methods)
    }

    /// Launches `id` with a fresh scoped token. The listener must run
    /// ([`Self::ensure_listener`]).
    ///
    /// # Errors
    ///
    /// [`TOOL_UNKNOWN`], [`TOOL_MANIFEST_INVALID`], [`TOOL_ALREADY_RUNNING`] or
    /// [`TOOL_SPAWN_FAILED`].
    pub fn launch(&mut self, id: &str, project: Option<&Path>) -> Result<(), ToolError> {
        let manifest = self.manifest(id)?.clone();
        let dir = self.find(id).map(DiscoveredTool::dir).unwrap_or_default();
        if self.status(id).is_live() {
            return Err(ToolError::new(
                TOOL_ALREADY_RUNNING,
                format!("tool `{id}` is running"),
            ));
        }
        let Some(endpoint) = self.endpoint else {
            return Err(ToolError::new(
                TOOL_SPAWN_FAILED,
                "the tool listener is not running",
            ));
        };
        let launches = self.runtimes.get(id).map_or(0, |r| r.launches) + 1;
        let mut runtime = ToolRuntime::new(launches);
        let (token, scope) = self.tokens.issue(Self::scope_for(&manifest));
        let request = SpawnRequest::new(&manifest, &dir, endpoint, &token, project);
        match request.spawn() {
            Ok(mut child) => {
                runtime.log_rx = Some(pipe_log(&mut child, id));
                runtime.child = Some(child);
                runtime.scope = Some(scope);
                runtime.status = ToolStatus::Running;
                tracing::info!(
                    target: "regia_editor",
                    "tool {id} launched: {} (pid {:?})",
                    request.program.display(),
                    runtime.child.as_ref().map(Child::id)
                );
                self.runtimes.insert(id.to_string(), runtime);
                Ok(())
            }
            Err(err) => {
                self.tokens.revoke(scope.id);
                let message = format!(
                    "cannot run {} in {}: {err}",
                    request.program.display(),
                    request.cwd.display()
                );
                runtime.push_log(message.clone());
                runtime.status = ToolStatus::Failed(message.clone());
                self.runtimes.insert(id.to_string(), runtime);
                Err(ToolError::new(TOOL_SPAWN_FAILED, message))
            }
        }
    }

    /// Asks `id` to stop: a `shutdown` event, then a kill after [`STOP_GRACE`] from `now` (at
    /// once with `force`).
    ///
    /// # Errors
    ///
    /// [`TOOL_NOT_RUNNING`].
    pub fn stop(&mut self, id: &str, force: bool, now: Instant) -> Result<(), ToolError> {
        let runtime = self.live_runtime(id)?;
        if force {
            runtime.kill();
            runtime.stop_deadline = Some(now);
        } else if runtime.status == ToolStatus::Running {
            runtime.queue.push_back(ToolEvent::Shutdown);
            runtime.stop_deadline = Some(now + STOP_GRACE);
            runtime.deliver();
        }
        runtime.status = ToolStatus::Stopping;
        Ok(())
    }

    fn live_runtime(&mut self, id: &str) -> Result<&mut ToolRuntime, ToolError> {
        match self.runtimes.get_mut(id) {
            Some(runtime) if runtime.status.is_live() => Ok(runtime),
            _ => Err(ToolError::new(
                TOOL_NOT_RUNNING,
                format!("tool `{id}` is not running"),
            )),
        }
    }

    fn running_runtime(&mut self, id: &str) -> Result<&mut ToolRuntime, ToolError> {
        match self.runtimes.get_mut(id) {
            Some(runtime) if runtime.status == ToolStatus::Running => Ok(runtime),
            _ => Err(ToolError::new(
                TOOL_NOT_RUNNING,
                format!("tool `{id}` is not running"),
            )),
        }
    }

    /// The runtime of the scoped `caller` (its current launch only).
    fn caller_runtime(&mut self, caller: &ToolScope) -> Result<&mut ToolRuntime, ToolError> {
        match self.runtimes.get_mut(&caller.tool) {
            Some(runtime) if runtime.scope.as_ref().is_some_and(|s| s.id == caller.id) => {
                runtime.requests += 1;
                Ok(runtime)
            }
            _ => Err(ToolError::new(
                regia_agent::scope::TOOL_TOKEN_REVOKED,
                format!("the token of tool {} belongs to an earlier launch", caller.tool),
            )),
        }
    }

    /// Queues a proxied call of `method` on `tool`; `reply` is answered by the tool's
    /// `editor.tool_reply`, or with [`TOOL_CALL_TIMEOUT`] at `now + timeout`. Returns the call id.
    /// Permissions are the caller's business (the agent method checks them).
    ///
    /// # Errors
    ///
    /// [`TOOL_UNKNOWN`], [`TOOL_MANIFEST_INVALID`], [`TOOL_UNKNOWN_METHOD`],
    /// [`TOOL_NOT_RUNNING`], or [`TOOL_ERROR`] when [`MAX_QUEUED`] events wait already.
    pub fn call(
        &mut self,
        tool: &str,
        method: &str,
        params: Value,
        timeout: Duration,
        reply: DeferredReply,
        now: Instant,
    ) -> Result<u64, ToolError> {
        if self.manifest(tool)?.provided(method).is_none() {
            return Err(ToolError::new(
                TOOL_UNKNOWN_METHOD,
                format!("tool `{tool}` does not provide {method}"),
            ));
        }
        self.next_call += 1;
        let call = self.next_call;
        let runtime = self.running_runtime(tool)?;
        if runtime.queue.len() >= MAX_QUEUED {
            return Err(ToolError::new(
                TOOL_ERROR,
                format!("tool `{tool}` has {MAX_QUEUED} undelivered events; it is not polling"),
            ));
        }
        runtime.calls.insert(
            call,
            PendingCall {
                reply,
                method: method.to_string(),
                deadline: now + timeout.min(MAX_CALL_TIMEOUT),
            },
        );
        runtime.queue.push_back(ToolEvent::Call {
            call,
            method: method.to_string(),
            params,
        });
        runtime.deliver();
        Ok(call)
    }

    /// Presses button `item` of `tool`'s view: a `click` event for the tool.
    ///
    /// # Errors
    ///
    /// [`TOOL_NOT_RUNNING`], `F0002` when the view has no such button, or [`TOOL_ERROR`] when
    /// the queue is full.
    pub fn click(&mut self, tool: &str, item: &str) -> Result<(), ToolError> {
        let runtime = self.running_runtime(tool)?;
        let known = runtime
            .view
            .iter()
            .any(|v| matches!(v, ViewItem::Button { id, .. } if id == item));
        if !known {
            return Err(bad_params(format!("tool `{tool}`'s view has no button `{item}`")));
        }
        if runtime.queue.len() >= MAX_QUEUED {
            return Err(ToolError::new(
                TOOL_ERROR,
                format!("tool `{tool}` is not polling"),
            ));
        }
        runtime.queue.push_back(ToolEvent::Click {
            item: item.to_string(),
        });
        runtime.deliver();
        Ok(())
    }

    /// The tool's poll: answered at once when work is queued (or `wait` is zero), else when
    /// work arrives or at `now + wait` with no events. A second poll answers the first empty.
    ///
    /// # Errors
    ///
    /// `tool_token_revoked` when `caller` is not the tool's current launch.
    pub fn poll(
        &mut self,
        caller: &ToolScope,
        wait: Duration,
        reply: DeferredReply,
        now: Instant,
    ) -> Result<(), ToolError> {
        let runtime = self.caller_runtime(caller)?;
        if let Some(previous) = runtime.poller.take() {
            previous.answer(&[]);
        }
        runtime.poller = Some(Poller {
            reply,
            deadline: now + wait.min(MAX_POLL_WAIT),
        });
        runtime.deliver();
        if wait.is_zero()
            && let Some(poller) = runtime.poller.take()
        {
            poller.answer(&[]);
        }
        Ok(())
    }

    /// The tool's answer to proxied call `call`.
    ///
    /// # Errors
    ///
    /// `tool_token_revoked`, or [`TOOL_UNKNOWN_CALL`] when the call is not open.
    pub fn reply(
        &mut self,
        caller: &ToolScope,
        call: u64,
        result: Result<Value, RpcError>,
    ) -> Result<(), ToolError> {
        let runtime = self.caller_runtime(caller)?;
        let Some(pending) = runtime.calls.remove(&call) else {
            return Err(ToolError::new(
                TOOL_UNKNOWN_CALL,
                format!("call {call} is not open"),
            ));
        };
        let _ = pending.reply.send(result);
        Ok(())
    }

    /// Updates (or adds) task `task.id` of the caller. Above [`MAX_TASKS`] the oldest finished
    /// task drops (the oldest task when none finished). Returns the stored task.
    ///
    /// # Errors
    ///
    /// `tool_token_revoked`.
    pub fn progress(&mut self, caller: &ToolScope, mut task: ToolTask) -> Result<ToolTask, ToolError> {
        let runtime = self.caller_runtime(caller)?;
        runtime.task_seq += 1;
        task.seq = runtime.task_seq;
        task.label = cut(task.label);
        task.message = cut(task.message);
        if task.label.is_empty() {
            task.label = task.id.clone();
        }
        match runtime.tasks.iter_mut().find(|t| t.id == task.id) {
            Some(existing) => *existing = task.clone(),
            None => runtime.tasks.push(task.clone()),
        }
        while runtime.tasks.len() > MAX_TASKS {
            let finished = runtime
                .tasks
                .iter()
                .enumerate()
                .filter(|(_, t)| t.state != TaskState::Running && t.id != task.id)
                .min_by_key(|(_, t)| t.seq)
                .map(|(i, _)| i);
            let oldest = runtime
                .tasks
                .iter()
                .enumerate()
                .filter(|(_, t)| t.id != task.id)
                .min_by_key(|(_, t)| t.seq)
                .map(|(i, _)| i);
            match finished.or(oldest) {
                Some(index) => {
                    runtime.tasks.remove(index);
                }
                None => break,
            }
        }
        Ok(task)
    }

    /// Replaces the caller's view.
    ///
    /// # Errors
    ///
    /// `tool_token_revoked`, or `F0002` above [`MAX_VIEW_ITEMS`] items.
    pub fn set_view(&mut self, caller: &ToolScope, items: Vec<ViewItem>) -> Result<usize, ToolError> {
        if items.len() > MAX_VIEW_ITEMS {
            return Err(bad_params(format!(
                "`items` holds {} items; at most {MAX_VIEW_ITEMS}",
                items.len()
            )));
        }
        let runtime = self.caller_runtime(caller)?;
        runtime.view = items;
        Ok(runtime.view.len())
    }

    /// One frame: reads the tools' pipes, reaps exited tools, kills tools past their stop grace,
    /// hands queued work to waiting polls, and answers expired polls and calls.
    pub fn tick(&mut self, now: Instant) {
        let tokens = self.tokens.clone();
        for (id, runtime) in &mut self.runtimes {
            runtime.drain_log();
            if runtime.status == ToolStatus::Stopping && runtime.stop_deadline.is_some_and(|d| now >= d) {
                runtime.kill();
            }
            let exited = match runtime.child.as_mut().map(Child::try_wait) {
                Some(Ok(Some(status))) => Some((status.code(), describe_exit(status))),
                Some(Ok(None)) | None => None,
                Some(Err(err)) => {
                    tracing::warn!(target: "regia_editor", "tool {id}: cannot read its exit status: {err}");
                    Some((None, "its exit status could not be read".to_string()))
                }
            };
            if let Some((code, description)) = exited {
                runtime.drain_log();
                // A failure is a warning and also lands in the tool's own log, so the panel
                // shows why it stopped; the editor itself carries on.
                if code == Some(0) {
                    tracing::info!(target: "regia_editor", "tool {id} exited ({description})");
                } else {
                    tracing::warn!(target: "regia_editor", "tool {id} stopped: {description}");
                    runtime.push_log(format!("[editor] tool stopped: {description}"));
                }
                runtime.finish(ToolStatus::Exited(code), &tokens);
                continue;
            }
            runtime.deliver();
            if runtime.poller.as_ref().is_some_and(|p| now >= p.deadline)
                && let Some(poller) = runtime.poller.take()
            {
                poller.answer(&[]);
            }
            let expired: Vec<u64> = runtime
                .calls
                .iter()
                .filter(|(_, call)| now >= call.deadline)
                .map(|(id, _)| *id)
                .collect();
            for call in expired {
                if let Some(pending) = runtime.calls.remove(&call) {
                    runtime
                        .queue
                        .retain(|e| !matches!(e, ToolEvent::Call { call: c, .. } if *c == call));
                    let error = ToolError::new(
                        TOOL_CALL_TIMEOUT,
                        format!("tool `{id}` did not answer {} in time", pending.method),
                    );
                    let _ = pending.reply.send(Err(error.rpc()));
                }
            }
        }
    }

    /// The ids of discovered tools that start with the editor and are not running.
    #[must_use]
    pub fn autostart_ids(&self) -> Vec<String> {
        self.discovered
            .iter()
            .filter(|tool| tool.autostarts())
            .map(DiscoveredTool::id)
            .filter(|id| !self.status(id).is_live())
            .collect()
    }

    /// The tools to autostart for `project`, once per project: empty without a project and on
    /// every later call for the same project (reading the manifests elsewhere does not use it up).
    pub fn take_autostart(&mut self, project: Option<&Path>) -> Vec<String> {
        let Some(project) = project else {
            return Vec::new();
        };
        if self.autostarted_for.as_deref() == Some(project) {
            return Vec::new();
        }
        self.autostarted_for = Some(project.to_path_buf());
        self.autostart_ids()
    }

    /// Kills every running tool and revokes every token (the editor is closing).
    pub fn shutdown_all(&mut self) {
        for runtime in self.runtimes.values_mut() {
            if runtime.status.is_live() {
                runtime.kill();
                if let Some(child) = runtime.child.as_mut() {
                    let _ = child.wait();
                }
                runtime.finish(ToolStatus::Exited(None), &self.tokens);
            }
        }
        self.tokens.revoke_all();
    }

    /// Adds a running tool with no process, token `scope` (tests of the protocol surface).
    #[cfg(test)]
    pub(crate) fn attach_for_test(&mut self, manifest: ToolManifest) -> Arc<ToolScope> {
        let (_, scope) = self.tokens.issue(Self::scope_for(&manifest));
        let mut runtime = ToolRuntime::new(1);
        runtime.scope = Some(Arc::clone(&scope));
        runtime.status = ToolStatus::Running;
        self.runtimes.insert(manifest.id.clone(), runtime);
        self.discovered.push(DiscoveredTool {
            path: PathBuf::from(format!("{}{}", manifest.id, super::manifest::MANIFEST_SUFFIX)),
            source: ManifestSource::User,
            manifest: Ok(manifest),
        });
        // Count the list as read for "no project", so handlers do not re-read the real dirs.
        self.discovered_for = Some(None);
        scope
    }

    /// Marks an attached test tool as exited, as [`Self::tick`] does for a real process.
    #[cfg(test)]
    pub(crate) fn exit_for_test(&mut self, id: &str, code: Option<i32>) {
        let tokens = self.tokens.clone();
        if let Some(runtime) = self.runtimes.get_mut(id) {
            runtime.finish(ToolStatus::Exited(code), &tokens);
        }
    }

    /// Blocks until a launched tool's process exits and both of its pipes close, then ticks
    /// (tests; no sleeping: the log channel disconnects when the reader threads end).
    #[cfg(test)]
    pub(crate) fn wait_exit_for_test(&mut self, id: &str) {
        if let Some(runtime) = self.runtimes.get_mut(id) {
            if let Some(child) = runtime.child.as_mut() {
                let _ = child.wait();
            }
            if let Some(rx) = runtime.log_rx.take()
                && let Ok(rx) = rx.into_inner()
            {
                for line in rx.iter() {
                    runtime.push_log(line);
                }
            }
        }
        self.tick(Instant::now());
    }
}

impl Drop for ToolHost {
    /// Tools never outlive the editor: running ones are killed and every token is revoked.
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit 11-03 editor M6: a non-UTF-8 line ended the reader, which closed the tool's pipe
    /// (the tool then died of SIGPIPE on its next write).
    #[test]
    fn a_non_utf8_log_line_does_not_end_the_reader() {
        let (tx, rx) = mpsc::channel();
        let bytes: &[u8] = b"first\n\xff\xfe binary progress\nafter\n";
        pump(bytes, &tx, "> ");
        drop(tx);
        let lines: Vec<String> = rx.iter().collect();
        assert_eq!(lines.len(), 3, "the reader stopped at the bad line: {lines:?}");
        assert_eq!(lines[0], "> first");
        assert!(lines[1].ends_with(" binary progress"), "{lines:?}");
        assert_eq!(lines[2], "> after");
    }

    fn manifest(id: &str) -> ToolManifest {
        ToolManifest::parse(&format!(
            "format = 1\nid = \"{id}\"\n[launch]\nprogram = \"p\"\n[scope]\npermissions = [\"play\"]\n\
             methods = [\"camera.*\"]\n[[provides]]\nname = \"pick.list\"\n"
        ))
        .expect("valid manifest")
    }

    fn reply() -> (
        DeferredReply,
        crossbeam_channel::Receiver<regia_agent::protocol::Response>,
    ) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        (DeferredReply::new(json!(1), tx), rx)
    }

    fn events(response: &regia_agent::protocol::Response) -> Vec<Value> {
        response.result.as_ref().expect("result")["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn a_launch_scope_adds_the_base_methods_and_read() {
        let scope = ToolHost::scope_for(&manifest("t"));
        assert!(scope.allows_method("camera.set_path"));
        assert!(scope.allows_method("editor.tool_poll"));
        assert!(scope.allows_method("agent.hello"));
        assert!(!scope.allows_method("scene.spawn"));
        assert!(scope.allows_permission(Permission::Read));
        assert!(scope.allows_permission(Permission::Play));
        assert!(!scope.allows_permission(Permission::Tools));
    }

    /// A proxied call reaches a waiting poll at once, the tool's reply answers the caller, and
    /// a second reply to the same call is refused.
    #[test]
    fn a_call_goes_to_the_poll_and_the_reply_comes_back() {
        let mut host = ToolHost::new();
        let scope = host.attach_for_test(manifest("t"));
        let now = Instant::now();
        let (poll, poll_rx) = reply();
        host.poll(&scope, Duration::from_secs(30), poll, now)
            .expect("polls");
        assert!(poll_rx.try_recv().is_err(), "waits for work");

        let (caller, caller_rx) = reply();
        let call = host
            .call(
                "t",
                "pick.list",
                json!({ "n": 3 }),
                DEFAULT_CALL_TIMEOUT,
                caller,
                now,
            )
            .expect("queued");
        let delivered = events(&poll_rx.try_recv().expect("poll answered"));
        assert_eq!(
            delivered,
            vec![json!({ "kind": "call", "call": call, "method": "pick.list", "params": { "n": 3 } })]
        );
        assert_eq!(host.runtime("t").map(ToolRuntime::calls_in_flight), Some(1));

        host.reply(&scope, call, Ok(json!({ "scenes": [] })))
            .expect("open call");
        let answer = caller_rx.try_recv().expect("caller answered");
        assert_eq!(answer.result, Some(json!({ "scenes": [] })));
        let again = host.reply(&scope, call, Ok(json!({}))).expect_err("closed");
        assert_eq!(again.code, TOOL_UNKNOWN_CALL);
    }

    /// Work queued before the poll is delivered by the poll itself; an empty zero-wait poll
    /// answers at once; a waiting poll answers empty at its deadline.
    #[test]
    fn polls_answer_queued_work_now_and_empty_at_their_deadline() {
        let mut host = ToolHost::new();
        let scope = host.attach_for_test(manifest("t"));
        let now = Instant::now();
        let (caller, _caller_rx) = reply();
        host.call("t", "pick.list", json!({}), DEFAULT_CALL_TIMEOUT, caller, now)
            .expect("queued");
        let (poll, poll_rx) = reply();
        host.poll(&scope, Duration::from_secs(10), poll, now)
            .expect("polls");
        assert_eq!(events(&poll_rx.try_recv().expect("at once")).len(), 1);

        let (poll, poll_rx) = reply();
        host.poll(&scope, Duration::ZERO, poll, now).expect("polls");
        assert!(events(&poll_rx.try_recv().expect("zero wait answers")).is_empty());

        let (poll, poll_rx) = reply();
        host.poll(&scope, Duration::from_secs(10), poll, now)
            .expect("polls");
        host.tick(now + Duration::from_secs(9));
        assert!(poll_rx.try_recv().is_err(), "before the deadline");
        host.tick(now + Duration::from_secs(10));
        assert!(events(&poll_rx.try_recv().expect("at the deadline")).is_empty());
    }

    #[test]
    fn an_unanswered_call_times_out_and_leaves_the_queue() {
        let mut host = ToolHost::new();
        host.attach_for_test(manifest("t"));
        let now = Instant::now();
        let (caller, caller_rx) = reply();
        host.call("t", "pick.list", json!({}), Duration::from_secs(5), caller, now)
            .expect("queued");
        host.tick(now + Duration::from_secs(4));
        assert!(caller_rx.try_recv().is_err());
        host.tick(now + Duration::from_secs(5));
        let error = caller_rx.try_recv().expect("timed out").error.expect("error");
        assert_eq!(error.code, protocol::TIMEOUT);
        assert_eq!(error.data.expect("data")["code"], TOOL_CALL_TIMEOUT);
        let runtime = host.runtime("t").expect("runtime");
        assert_eq!(runtime.calls_in_flight(), 0);
        assert_eq!(runtime.queued(), 0, "the undelivered call is dropped");
    }

    #[test]
    fn calls_need_a_declared_method_and_a_running_tool() {
        let mut host = ToolHost::new();
        host.attach_for_test(manifest("t"));
        let now = Instant::now();
        let (caller, _) = reply();
        let error = host
            .call("t", "nope", json!({}), DEFAULT_CALL_TIMEOUT, caller, now)
            .expect_err("undeclared");
        assert_eq!(error.code, TOOL_UNKNOWN_METHOD);
        let (caller, _) = reply();
        let error = host
            .call("ghost", "pick.list", json!({}), DEFAULT_CALL_TIMEOUT, caller, now)
            .expect_err("unknown tool");
        assert_eq!(error.code, TOOL_UNKNOWN);
        host.stop("t", false, now).expect("stops");
        let (caller, _) = reply();
        let error = host
            .call("t", "pick.list", json!({}), DEFAULT_CALL_TIMEOUT, caller, now)
            .expect_err("stopping");
        assert_eq!(error.code, TOOL_NOT_RUNNING);
    }

    /// When the tool exits, open calls answer `tool_exited`, its poll answers empty, and its
    /// token is revoked; a later request on that scope is refused as revoked.
    #[test]
    fn an_exit_fails_open_calls_and_revokes_the_token() {
        let mut host = ToolHost::new();
        let scope = host.attach_for_test(manifest("t"));
        let now = Instant::now();
        let (caller, caller_rx) = reply();
        host.call("t", "pick.list", json!({}), DEFAULT_CALL_TIMEOUT, caller, now)
            .expect("queued");
        assert!(host.tokens().is_live(scope.id));
        host.exit_for_test("t", Some(3));
        let error = caller_rx.try_recv().expect("answered").error.expect("error");
        assert_eq!(error.data.expect("data")["code"], TOOL_EXITED);
        assert!(!host.tokens().is_live(scope.id));
        assert_eq!(host.status("t"), ToolStatus::Exited(Some(3)));
        let (poll, _) = reply();
        let error = host.poll(&scope, Duration::ZERO, poll, now).expect_err("revoked");
        assert_eq!(error.code, regia_agent::scope::TOOL_TOKEN_REVOKED);
    }

    #[test]
    fn stop_sends_shutdown_then_kills_after_the_grace() {
        let mut host = ToolHost::new();
        let scope = host.attach_for_test(manifest("t"));
        let now = Instant::now();
        let (poll, poll_rx) = reply();
        host.poll(&scope, Duration::from_secs(30), poll, now)
            .expect("polls");
        host.stop("t", false, now).expect("stops");
        assert_eq!(
            events(&poll_rx.try_recv().expect("delivered")),
            vec![json!({ "kind": "shutdown" })]
        );
        assert_eq!(host.status("t"), ToolStatus::Stopping);
        let error = host.stop("ghost", false, now).expect_err("not running");
        assert_eq!(error.code, TOOL_NOT_RUNNING);
    }

    #[test]
    fn clicks_need_a_button_of_the_view() {
        let mut host = ToolHost::new();
        let scope = host.attach_for_test(manifest("t"));
        let error = host.click("t", "approve").expect_err("no view yet");
        assert_eq!(error.code, "F0002");
        let items = vec![
            ViewItem::Heading("Queue".into()),
            ViewItem::Button {
                id: "approve".into(),
                label: "Approve".into(),
            },
        ];
        assert_eq!(host.set_view(&scope, items).expect("set"), 2);
        host.click("t", "approve").expect("clicked");
        let (poll, poll_rx) = reply();
        host.poll(&scope, Duration::ZERO, poll, Instant::now())
            .expect("polls");
        assert_eq!(
            events(&poll_rx.try_recv().expect("answered")),
            vec![json!({ "kind": "click", "item": "approve" })]
        );
        let too_many = vec![ViewItem::Separator; MAX_VIEW_ITEMS + 1];
        assert_eq!(
            host.set_view(&scope, too_many).expect_err("too many").code,
            "F0002"
        );
    }

    fn task(id: &str, state: TaskState) -> ToolTask {
        ToolTask {
            id: id.into(),
            label: String::new(),
            fraction: Some(0.5),
            state,
            message: String::new(),
            seq: 0,
        }
    }

    /// Updates replace a task by id; above the cap the oldest finished task drops first, and a
    /// running task stays.
    #[test]
    fn tasks_update_in_place_and_drop_oldest_finished_first() {
        let mut host = ToolHost::new();
        let scope = host.attach_for_test(manifest("t"));
        host.progress(&scope, task("keep", TaskState::Running))
            .expect("ok");
        for n in 0..MAX_TASKS {
            host.progress(&scope, task(&format!("done-{n}"), TaskState::Done))
                .expect("ok");
        }
        let tasks = host.runtime("t").expect("runtime").tasks().to_vec();
        assert_eq!(tasks.len(), MAX_TASKS);
        assert!(tasks.iter().any(|t| t.id == "keep"), "the running task stays");
        assert!(
            !tasks.iter().any(|t| t.id == "done-0"),
            "the oldest finished task dropped"
        );
        let updated = host.progress(&scope, task("keep", TaskState::Done)).expect("ok");
        assert_eq!(updated.label, "keep", "the label defaults to the id");
        let tasks = host.runtime("t").expect("runtime").tasks().to_vec();
        assert_eq!(tasks.len(), MAX_TASKS);
        assert_eq!(tasks.iter().filter(|t| t.id == "keep").count(), 1);
        assert!(!host.is_busy());
    }

    #[test]
    fn view_items_parse_and_round_trip() {
        let items = [
            json!({ "kind": "heading", "text": "Review queue" }),
            json!({ "kind": "label", "text": "3 scenes waiting" }),
            json!({ "kind": "value", "label": "Daily cap", "value": "40" }),
            json!({ "kind": "progress", "label": "Upload", "fraction": 0.25 }),
            json!({ "kind": "button", "id": "approve", "label": "Approve" }),
            json!({ "kind": "link", "label": "Pick page", "url": "http://127.0.0.1:8765/" }),
            json!({ "kind": "separator" }),
        ];
        for item in &items {
            let parsed = ViewItem::parse(item).expect("valid");
            assert_eq!(&parsed.to_json(), item);
        }
        let clamped = ViewItem::parse(&json!({ "kind": "progress", "fraction": 7.0 })).expect("valid");
        assert_eq!(
            clamped,
            ViewItem::Progress {
                label: String::new(),
                fraction: 1.0
            }
        );
        for bad in [
            json!({ "text": "no kind" }),
            json!({ "kind": "marquee" }),
            json!({ "kind": "label" }),
            json!({ "kind": "label", "text": 5 }),
            json!({ "kind": "progress", "fraction": "half" }),
            json!({ "kind": "button", "id": "" }),
            json!({ "kind": "link", "url": "file:///etc/passwd" }),
        ] {
            assert!(ViewItem::parse(&bad).is_err(), "{bad}");
        }
        let long =
            ViewItem::parse(&json!({ "kind": "label", "text": "é".repeat(MAX_TEXT_BYTES) })).expect("valid");
        let ViewItem::Label(text) = long else {
            panic!("a label");
        };
        assert!(text.len() <= MAX_TEXT_BYTES, "cut on a char boundary");
    }

    /// The env a launch passes: the manifest's variables, the scoped endpoint and token, the
    /// id, the protocol and the project; the program and cwd resolve against the manifest.
    #[test]
    fn a_spawn_request_carries_the_token_and_the_project() {
        let manifest = ToolManifest::parse(
            "format = 1\nid = \"t\"\n[launch]\nprogram = \"bin/tool\"\ncwd = \"work\"\nargs = [\"--panel\"]\n\
             env = { MODE = \"editor\" }\n",
        )
        .expect("valid");
        let addr: SocketAddr = "127.0.0.1:4567".parse().expect("addr");
        let request = SpawnRequest::new(
            &manifest,
            Path::new("dir"),
            addr,
            "secret",
            Some(Path::new("proj")),
        );
        assert_eq!(request.program, Path::new("dir").join("bin/tool"));
        assert_eq!(request.cwd, Path::new("dir").join("work"));
        assert_eq!(request.args, ["--panel"]);
        let env: BTreeMap<&str, &str> = request
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(env["MODE"], "editor");
        assert_eq!(env[ENDPOINT_ENV], "ws://127.0.0.1:4567");
        assert_eq!(env[TOKEN_ENV], "secret");
        assert_eq!(env[ID_ENV], "t");
        assert_eq!(env[PROTOCOL_ENV], protocol::PROTOCOL_VERSION);
        assert_eq!(env[PROJECT_DIR_ENV], "proj");
    }

    /// A real launch: the test binary itself, asked for `--help`, starts, prints, exits 0; its
    /// output lands in the log and its token is revoked. A missing program fails cleanly and
    /// revokes the token it was given.
    #[test]
    fn a_real_process_launches_logs_and_exits() {
        let root = regia_test_utils::unique_temp_dir("tool-launch");
        let exe = std::env::current_exe().expect("test binary");
        let text = format!(
            "format = 1\nid = \"selftest\"\n[launch]\nprogram = {:?}\nargs = [\"--help\"]\n",
            exe.display().to_string()
        );
        std::fs::write(root.join("selftest.tool.toml"), text).expect("manifest");
        std::fs::write(
            root.join("missing.tool.toml"),
            "format = 1\nid = \"missing\"\n[launch]\nprogram = \"./no-such-program-here\"\n",
        )
        .expect("manifest");
        let mut host = ToolHost::new();
        host.refresh_from(&[(root.clone(), ManifestSource::ToolPath)]);
        let launch_before_listener = host.launch("selftest", None).expect_err("no listener");
        assert_eq!(launch_before_listener.code, TOOL_SPAWN_FAILED);
        assert!(host.ensure_listener(None).expect("listener").is_some());
        assert!(
            host.ensure_listener(None).expect("listener").is_none(),
            "started once"
        );

        host.launch("selftest", None).expect("launches");
        assert_eq!(host.status("selftest"), ToolStatus::Running);
        assert_eq!(host.tokens().len(), 1);
        assert_eq!(
            host.launch("selftest", None).expect_err("running").code,
            TOOL_ALREADY_RUNNING
        );
        host.wait_exit_for_test("selftest");
        assert_eq!(host.status("selftest"), ToolStatus::Exited(Some(0)));
        assert!(host.tokens().is_empty(), "the token is revoked at exit");
        let log = host.runtime("selftest").expect("runtime").log_tail(LOG_LINES);
        assert!(!log.is_empty(), "the child's output is logged");

        let error = host.launch("missing", None).expect_err("no program");
        assert_eq!(error.code, TOOL_SPAWN_FAILED);
        assert!(matches!(host.status("missing"), ToolStatus::Failed(_)));
        assert!(host.tokens().is_empty(), "a failed launch revokes its token");
        assert_eq!(
            host.launch("ghost", None).expect_err("unknown").code,
            TOOL_UNKNOWN
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tool process for the tests below: this test binary launched with a filter. Does
    /// nothing in an ordinary run (the launcher's id variable is absent).
    #[test]
    fn tool_child_exit_one() {
        if std::env::var_os(ID_ENV).is_some() {
            std::process::exit(1);
        }
    }

    /// Like `tool_child_exit_one`, but never exits by itself: the host has to kill it.
    #[test]
    fn tool_child_hang() {
        if std::env::var_os(ID_ENV).is_some() {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    }

    fn launch_test_child(host: &mut ToolHost, root: &Path, id: &str, filter: &str) {
        let exe = std::env::current_exe().expect("test binary");
        let text = format!(
            "format = 1\nid = \"{id}\"\n[launch]\nprogram = {:?}\nargs = [\"{filter}\", \"--test-threads\", \"1\"]\n",
            exe.display().to_string()
        );
        std::fs::write(root.join(format!("{id}.tool.toml")), text).expect("manifest");
        host.refresh_from(&[(root.to_path_buf(), ManifestSource::ToolPath)]);
        host.ensure_listener(None).expect("listener");
        host.launch(id, None).expect("launches");
        assert_eq!(host.status(id), ToolStatus::Running);
    }

    /// A tool that exits 1 is marked exited with that code, says why in its log, loses its
    /// token, and the host (the editor) carries on: the same tool launches again.
    #[test]
    fn a_tool_that_exits_one_is_marked_stopped_with_its_code() {
        let root = regia_test_utils::unique_temp_dir("tool-exit-one");
        let mut host = ToolHost::new();
        launch_test_child(&mut host, &root, "failing", "tool_child_exit_one");
        host.wait_exit_for_test("failing");
        assert_eq!(host.status("failing"), ToolStatus::Exited(Some(1)));
        assert!(host.tokens().is_empty(), "the token is revoked at exit");
        let log = host.runtime("failing").expect("runtime").log_tail(LOG_LINES);
        assert!(
            log.iter().any(|line| line.contains("tool stopped: exit code 1")),
            "{log:?}"
        );
        host.launch("failing", None)
            .expect("a stopped tool launches again");
        host.wait_exit_for_test("failing");
        assert_eq!(host.status("failing"), ToolStatus::Exited(Some(1)));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tool that is killed (here by a forced stop) ends as exited, never as an error in the
    /// host; where the platform reports a signal instead of a code, the log names it.
    #[test]
    fn a_killed_tool_is_marked_stopped() {
        let root = regia_test_utils::unique_temp_dir("tool-killed");
        let mut host = ToolHost::new();
        launch_test_child(&mut host, &root, "hanging", "tool_child_hang");
        host.stop("hanging", true, Instant::now()).expect("stops");
        host.wait_exit_for_test("hanging");
        let status = host.status("hanging");
        assert!(
            matches!(status, ToolStatus::Exited(code) if code != Some(0)),
            "{status:?}"
        );
        assert!(host.tokens().is_empty(), "the token is revoked at exit");
        let log = host.runtime("hanging").expect("runtime").log_tail(LOG_LINES);
        assert!(log.iter().any(|line| line.contains("tool stopped:")), "{log:?}");
        #[cfg(unix)]
        assert!(
            log.iter().any(|line| line.contains("killed by signal")),
            "{log:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Autostart happens once per opened project, whoever read the manifests first, and never
    /// without a project.
    #[test]
    fn autostart_is_taken_once_per_project() {
        let root = regia_test_utils::unique_temp_dir("tool-autostart");
        std::fs::write(
            root.join("auto.tool.toml"),
            "format = 1\nid = \"auto\"\n[launch]\nprogram = \"p\"\nautostart = true\n",
        )
        .expect("manifest");
        let mut host = ToolHost::new();
        host.refresh_from(&[(root.clone(), ManifestSource::User)]);
        assert!(host.take_autostart(None).is_empty(), "no project, no autostart");
        let project = root.join("project");
        assert_eq!(host.take_autostart(Some(project.as_path())), ["auto"]);
        assert!(
            host.take_autostart(Some(project.as_path())).is_empty(),
            "once per project"
        );
        assert_eq!(
            host.take_autostart(Some(root.join("other").as_path())),
            ["auto"],
            "a new project"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn errors_map_to_their_protocol_class() {
        for (code, class) in [
            (TOOL_UNKNOWN, protocol::NOT_FOUND),
            (TOOL_MANIFEST_INVALID, protocol::INVALID_PARAMS),
            (TOOL_ALREADY_RUNNING, protocol::INVALID_REQUEST),
            (TOOL_NOT_RUNNING, protocol::INVALID_REQUEST),
            (TOOL_SPAWN_FAILED, protocol::INTERNAL_ERROR),
            (TOOL_UNKNOWN_METHOD, protocol::NOT_FOUND),
            (TOOL_CALL_TIMEOUT, protocol::TIMEOUT),
            (TOOL_EXITED, protocol::INTERNAL_ERROR),
            (TOOL_ERROR, protocol::INTERNAL_ERROR),
            (TOOL_CALLER_REQUIRED, protocol::INVALID_REQUEST),
            (TOOL_UNKNOWN_CALL, protocol::NOT_FOUND),
            (
                regia_agent::scope::TOOL_TOKEN_REVOKED,
                protocol::PERMISSION_DENIED,
            ),
            ("F0002", protocol::INVALID_PARAMS),
        ] {
            let error = ToolError::new(code, "x").rpc();
            assert_eq!(error.code, class, "{code}");
            assert_eq!(error.data.expect("data")["code"], code);
        }
    }
}
