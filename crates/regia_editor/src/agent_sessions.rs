// SPDX-License-Identifier: MPL-2.0
//! User-configured hosted agents (P-368–370, ADR-7844).
//!
//! The editor starts no service: commands are explicit argument vectors, launched in the
//! project with inherited user privileges. Each gets a private scoped token in its environment.
//! Stdout is bounded JSON lines (`message` or `plan`); stdin carries user messages, mode changes
//! and plan decisions. Requests use the ordinary agent protocol and dispatcher.
//!
//! Propose mode stages live `editor.*` edits in a session-only queue (P-369, ADR-8375): each
//! is held with its parameters and a preview until the owner approves (all or per item) or
//! rejects; approved calls run in order through the dispatcher as one undo entry.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use regia_agent::protocol::{PERMISSION_DENIED, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_agent::scope::{ToolScope, ToolTokens};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_LINE: usize = 16384;
const MAX_TEXT: usize = 8192;
const MAX_MESSAGES: usize = 256;
const MAX_SESSIONS: usize = 16;
const MAX_STEPS: usize = 32;
/// Most calls one session may hold staged; also bounds one approval's dispatcher work.
const MAX_STAGED: usize = 32;
/// The guard's private staging signal. The tick turns it into a `staged` reply; no caller sees it.
const STAGE_SIGNAL: &str = "session_stage_signal";
/// Editor calls that manage history or replace the world: they cannot run inside the approval's
/// undo group (`undo_agent::transaction_guard` refuses them while a transaction is open), so
/// Propose refuses them instead of staging them.
const UNSTAGEABLE: &[&str] = &[
    "editor.transaction_begin",
    "editor.transaction_commit",
    "editor.transaction_abort",
    "editor.undo",
    "editor.redo",
    "editor.history_jump",
    "editor.tables_panel_undo",
    "editor.vehicle_tuning_undo",
    "editor.vehicle_tuning_redo",
    "editor.open_level",
    "editor.open_project",
    "editor.new_project",
    "editor.open_prefab",
    "editor.close_prefab",
    "editor.placed_edit",
    "editor.recovery_restore",
    "editor.command",
    "editor.menu_invoke",
];

/// Per-session permissions, always intersected with the project's permissions.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Only the Read permission class.
    #[default]
    Ask,
    /// File edits go through `review.propose`; live `editor.*` edits are staged for approval.
    Propose,
    /// The project's normal permissions.
    Act,
}

impl Mode {
    /// User-facing mode label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask only",
            Self::Propose => "Propose",
            Self::Act => "Act",
        }
    }
}

/// An executable and its arguments from Preferences; never interpreted by a shell.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCommand {
    /// Display name (unique in the command list).
    pub name: String,
    /// Executable followed by arguments. Credentials belong in the external agent's environment.
    pub command: Vec<String>,
}

fn error(code: &str, message: &str) -> RpcError {
    RpcError::new(
        regia_agent::protocol::INVALID_PARAMS,
        code,
        message,
        "See book/agents/agent-panel.md",
    )
}

fn denied(code: &str, message: &str) -> RpcError {
    RpcError::new(
        PERMISSION_DENIED,
        code,
        message,
        "Use the Agent panel to review the session mode and plan",
    )
}

/// Parses and validates the configured command list. Empty text disables hosting.
pub fn commands(world: &World) -> Result<Vec<AgentCommand>, RpcError> {
    let text = world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .map(|p| p.text("agents.commands"))
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let commands: Vec<AgentCommand> = serde_json::from_str(&text).map_err(|_| {
        error(
            "session_bad_commands",
            "Agent commands must be a JSON list of named argument vectors",
        )
    })?;
    let mut names = std::collections::BTreeSet::new();
    if commands.len() > MAX_SESSIONS {
        return Err(error(
            "session_bad_commands",
            "At most 16 agent commands are allowed",
        ));
    }
    for command in &commands {
        if command.name.trim().is_empty()
            || command.name.len() > 128
            || command.name.chars().any(char::is_control)
            || !names.insert(&command.name)
            || command.command.is_empty()
            || command.command.len() > 64
            || command.command[0].is_empty()
            || command.command.iter().any(|a| a.contains('\0'))
        {
            return Err(error(
                "session_bad_commands",
                "Each command needs a unique name and a nonempty executable",
            ));
        }
        if command.command.iter().any(|arg| sensitive(arg)) {
            return Err(error(
                "session_credentials",
                "Keep credentials in the external agent's environment, not command arguments",
            ));
        }
    }
    Ok(commands)
}

/// Text shown in the panel and stored in its history; mode is captured when the entry arrives.
#[derive(Clone, Serialize)]
pub(crate) struct Message {
    pub mode: Mode,
    pub role: &'static str,
    pub text: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct Plan {
    pub steps: Vec<String>,
    pub approved: bool,
    pub revision: u64,
}

/// One live editor call a Propose session staged: run only when the owner approves it.
struct Staged {
    id: u64,
    method: String,
    params: Value,
    /// The calling scope, so an approved call keeps the session's identity, audit and provenance.
    scope: Arc<ToolScope>,
    /// `{summary, permission, touches}`, redacted for display.
    preview: Value,
    /// The parameters as redacted display text.
    params_text: String,
}

impl Staged {
    fn snapshot(&self) -> Value {
        json!({"id": self.id, "method": self.method, "params": self.params_text, "preview": self.preview})
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Output {
    Message { text: String },
    Plan { steps: Vec<String> },
}

enum ChildEvent {
    Output(Output),
    End,
    Fault,
}
enum WriteTask {
    Input(Vec<u8>),
    History,
}

struct Process {
    child: Child,
    group: Option<regia_platform::child_bind::ChildGroup>,
    input: Option<mpsc::SyncSender<WriteTask>>,
    events: Mutex<mpsc::Receiver<ChildEvent>>,
    threads: Vec<JoinHandle<()>>,
    failed: Arc<AtomicBool>,
    latest_history: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Process {
    fn stop(&mut self) {
        // Match launch.rs: kill the group before reaping its leader (no PID reuse window).
        #[cfg(unix)]
        {
            let _ = regia_platform::child_bind::kill_process_group(self.child.id());
        }
        self.group.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.input.take();
        // Readers use try_send, so a full event queue cannot keep shutdown waiting on the UI.
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Session {
    id: u64,
    name: String,
    mode: Mode,
    token: String,
    process: Option<Process>,
    messages: VecDeque<Message>,
    plan: Option<Plan>,
    history: PathBuf,
    root: PathBuf,
    history_error: bool,
    history_dirty: bool,
    secrets: Vec<String>,
    /// The last drain stopped at its bound, so stdout may still hold a plan. Requests from this
    /// session wait until the backlog is read, so a plan always closes the gate before dispatch.
    output_backlog: bool,
    /// Propose-mode editor calls awaiting the owner, oldest first (session-only, never saved).
    staged: Vec<Staged>,
    /// The id the next staged call gets.
    next_staged: u64,
}

impl Session {
    fn snapshot(&self) -> Value {
        json!({"id": self.id, "name": self.name, "mode": self.mode,
            "running": self.process.is_some(), "messages": self.messages,
            "plan": self.plan, "history": self.history.to_string_lossy(), "history_error": self.history_error,
            "staged": self.staged.iter().map(Staged::snapshot).collect::<Vec<_>>()})
    }
    fn redact(&self, text: &str) -> String {
        if sensitive(text)
            || self
                .secrets
                .iter()
                .any(|secret| secret.len() < 8 && text.contains(secret))
        {
            return "[credential-bearing text omitted]".into();
        }
        let mut text = text.to_string();
        for secret in &self.secrets {
            text = text.replace(secret, "[redacted]");
        }
        text.replace(&self.token, "[redacted]")
    }
    fn record(&mut self, role: &'static str, text: &str) {
        self.messages.push_back(Message {
            mode: self.mode,
            role,
            text: self.redact(text),
        });
        while self.messages.len() > MAX_MESSAGES {
            self.messages.pop_front();
        }
        self.history_dirty = true;
    }
    fn flush_history(&mut self) {
        if !self.history_dirty {
            return;
        }
        self.history_dirty = false;
        let Some(bytes) = self.history_bytes() else {
            self.history_error = true;
            return;
        };
        if let Some(process) = &self.process {
            match process.latest_history.lock() {
                Ok(mut latest) => *latest = Some(bytes),
                Err(_) => {
                    self.history_error = true;
                    return;
                }
            }
            // Coalesce snapshots: even a full input queue writes the newest snapshot when
            // its next item completes. Never queue 32 copies of a large transcript.
            if self.queue(WriteTask::History).is_err() {
                self.history_error = true;
            }
        }
    }
    fn history_bytes(&self) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        for message in &self.messages {
            if serde_json::to_writer(
                &mut bytes,
                &json!({"format_version":1, "session": self.id, "agent": self.name, "message": message}),
            )
            .is_err()
            {
                return None;
            }
            bytes.push(b'\n');
        }
        Some(bytes)
    }
    fn stop(&mut self) {
        self.process.take();
        self.output_backlog = false;
        // The queue is session-only: a stopped agent's staged calls never run.
        if !self.staged.is_empty() {
            let count = self.staged.len();
            self.staged.clear();
            self.record("system", &format!("Discarded {count} staged calls"));
        }
        self.history_dirty = false;
        // Workers have joined: flush the newest bounded snapshot even if their queue filled.
        if let Some(bytes) = self.history_bytes() {
            self.history_error = write_history(&self.history, &bytes).is_err();
        } else {
            self.history_error = true;
        }
    }
    fn queue(&self, task: WriteTask) -> Result<(), RpcError> {
        let sender = self
            .process
            .as_ref()
            .and_then(|p| p.input.as_ref())
            .ok_or_else(|| error("session_stopped", "The agent is stopped"))?;
        sender
            .try_send(task)
            .map_err(|_| error("session_busy", "The agent's input queue is full or closed"))
    }
    fn send(&self, value: Value) -> Result<(), RpcError> {
        let mut bytes =
            serde_json::to_vec(&value).map_err(|_| error("session_io", "Cannot encode agent input"))?;
        bytes.push(b'\n');
        self.queue(WriteTask::Input(bytes))
    }
}

/// Hosted children and their private protocol listener. No listener exists until Start.
#[derive(Resource, Default)]
pub struct AgentSessions {
    sessions: BTreeMap<u64, Session>,
    tokens: ToolTokens,
    server: Option<regia_agent::server::ScopedServer>,
    wake: Option<regia_agent::server::WakeHook>,
    /// A request taken from the inbox whose session still had unread stdout. It is dispatched
    /// first on a later tick, keeping the inbox order.
    held: Option<regia_agent::scope::ScopedInbound>,
}

impl Drop for AgentSessions {
    fn drop(&mut self) {
        self.tokens.revoke_all();
        for session in self.sessions.values_mut() {
            session.stop();
        }
    }
}

/// Installs an event-loop wake callback; closed or idle panels do not poll or repaint.
pub fn set_wake(world: &mut World, ctx: &egui::Context) {
    let ctx = ctx.clone();
    let wake = world
        .get_resource::<crate::spacemouse::SpaceMouseWaker>()
        .map(|waker| Arc::clone(&waker.0));
    let mut sessions = world.get_resource_or_insert_with::<AgentSessions>(Default::default);
    if sessions.wake.is_none() {
        sessions.wake = wake;
    }
    if sessions.wake.is_none() {
        sessions.wake = Some(Arc::new(move || ctx.request_repaint()));
    }
}

pub(crate) fn sensitive(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "bearer ",
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "access-token",
        "password",
        "secret=",
        "token=",
        "sk-",
        "ghp_",
        "github_pat_",
    ]
    .iter()
    .any(|part| lower.contains(part))
}

fn notify(
    events: &mpsc::SyncSender<ChildEvent>,
    event: ChildEvent,
    wake: &Option<regia_agent::server::WakeHook>,
    failed: &AtomicBool,
) {
    if events.try_send(event).is_err() {
        failed.store(true, Ordering::Release);
    }
    if let Some(wake) = wake {
        wake();
    }
}

fn write_history(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    // A failed create_new never follows or removes an unexpected file or symlink.
    let mut file = options.open(&tmp)?;
    let result = file.write_all(bytes);
    drop(file);
    let result = result.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn spawn(
    command: &AgentCommand,
    root: &Path,
    url: &str,
    token: &str,
    id: u64,
    history: &Path,
    wake: Option<regia_agent::server::WakeHook>,
) -> Result<Process, RpcError> {
    let mut cmd = ProcessCommand::new(&command.command[0]);
    cmd.args(&command.command[1..])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env(regia_agent::scope::ENDPOINT_ENV, url)
        .env(regia_agent::scope::TOKEN_ENV, token)
        .env(regia_agent::scope::ID_ENV, format!("agent-session-{id}"))
        .env("REGIA_AGENT_LINE_PROTOCOL", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    regia_platform::child_bind::bind_to_parent(&mut cmd);
    let group = regia_platform::child_bind::ChildGroup::new();
    let spawned = group
        .spawn(&mut cmd)
        .map_err(|_| error("session_spawn", "Cannot start the configured agent executable"))?;
    let (event_tx, event_rx) = mpsc::sync_channel(64);
    let mut process = Process {
        child: spawned.child,
        group: Some(group),
        input: None,
        events: Mutex::new(event_rx),
        threads: Vec::new(),
        failed: Arc::new(AtomicBool::new(false)),
        latest_history: Arc::new(Mutex::new(None)),
    };
    if spawned.outside.is_some() {
        return Err(error(
            "session_spawn",
            "Cannot bind the agent to the editor's process group",
        ));
    }
    let Some(mut stdin) = process.child.stdin.take() else {
        return Err(error("session_spawn", "No agent input pipe"));
    };
    let Some(stdout) = process.child.stdout.take() else {
        return Err(error("session_spawn", "No agent output pipe"));
    };
    let (input, tasks) = mpsc::sync_channel(32);
    process.input = Some(input);
    let path = history.to_path_buf();
    let writer_events = event_tx.clone();
    let writer_failed = Arc::clone(&process.failed);
    let reader_failed = Arc::clone(&process.failed);
    let writer_wake = wake.clone();
    let latest_history = Arc::clone(&process.latest_history);
    process.threads.push(
        std::thread::Builder::new()
            .name("regia-agent-input".into())
            .spawn(move || {
                for task in tasks {
                    let result = match task {
                        WriteTask::Input(bytes) => stdin.write_all(&bytes).and_then(|()| stdin.flush()),
                        WriteTask::History => Ok(()),
                    };
                    if result.is_err() {
                        notify(&writer_events, ChildEvent::Fault, &writer_wake, &writer_failed);
                    }
                    let bytes = latest_history.lock().ok().and_then(|mut latest| latest.take());
                    if let Some(bytes) = bytes
                        && write_history(&path, &bytes).is_err()
                    {
                        notify(&writer_events, ChildEvent::Fault, &writer_wake, &writer_failed);
                    }
                }
            })
            .map_err(|_| error("session_spawn", "Cannot start the agent input worker"))?,
    );
    process.threads.push(
        std::thread::Builder::new()
            .name("regia-agent-output".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut line = Vec::new();
                    match reader
                        .by_ref()
                        .take((MAX_LINE + 1) as u64)
                        .read_until(b'\n', &mut line)
                    {
                        Ok(0) => {
                            notify(&event_tx, ChildEvent::End, &wake, &reader_failed);
                            break;
                        }
                        Ok(_) if line.len() <= MAX_LINE => match serde_json::from_slice::<Output>(&line) {
                            Ok(output) => {
                                notify(&event_tx, ChildEvent::Output(output), &wake, &reader_failed);
                            }
                            Err(_) => {
                                let known_kind =
                                    serde_json::from_slice::<Value>(&line).ok().is_some_and(|value| {
                                        matches!(value["kind"].as_str(), Some("message" | "plan"))
                                    });
                                if known_kind {
                                    notify(&event_tx, ChildEvent::Fault, &wake, &reader_failed);
                                    break;
                                }
                            }
                        },
                        _ => {
                            notify(&event_tx, ChildEvent::Fault, &wake, &reader_failed);
                            break;
                        }
                    }
                }
            })
            .map_err(|_| error("session_spawn", "Cannot start the agent output worker"))?,
    );
    Ok(process)
}

fn start(world: &mut World, name: &str, mode: Mode) -> Result<Value, RpcError> {
    let command = commands(world)?
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            error(
                "session_not_configured",
                "Configure this agent command in Preferences first",
            )
        })?;
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| error("session_no_project", "Open a project before starting an agent"))?;
    let wake = world
        .get_resource::<crate::spacemouse::SpaceMouseWaker>()
        .map(|waker| Arc::clone(&waker.0));
    let mut sessions = world.get_resource_or_insert_with::<AgentSessions>(Default::default);
    if sessions.wake.is_none() {
        sessions.wake = wake;
    }
    if sessions.sessions.len() >= MAX_SESSIONS {
        let old = sessions
            .sessions
            .iter()
            .find(|(_, s)| s.process.is_none())
            .map(|(id, _)| *id);
        if let Some(id) = old {
            sessions.sessions.remove(&id);
        } else {
            return Err(error("session_limit", "Stop an agent before starting another"));
        }
    }
    if sessions.server.is_none() {
        sessions.server = Some(
            regia_agent::server::start_scoped(sessions.tokens.clone(), sessions.wake.clone())
                .map_err(|_| error("session_io", "Cannot start the private loopback listener"))?,
        );
    }
    let Some(server) = &sessions.server else {
        return Err(error("session_io", "No session listener"));
    };
    let url = format!("ws://{}", server.addr);
    let (token, scope) = sessions.tokens.issue(ToolScope::new(
        format!("hosted-agent:{}", command.name),
        [
            Permission::Read,
            Permission::EditScene,
            Permission::Assets,
            Permission::Play,
        ],
        vec!["*".into()],
    ));
    let id = scope.id;
    let relative = format!(
        ".regia/agent_sessions/{}-{}-{id}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis())
    );
    let history = match regia_core::project_path::resolve_write(&root, &relative) {
        Ok(path) => path,
        Err(_) => {
            sessions.tokens.revoke(id);
            return Err(error(
                "session_io",
                "The agent history path is outside the project",
            ));
        }
    };
    let result = (|| {
        if let Some(parent) = history.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| error("session_io", "Cannot create agent history directory"))?;
        }
        // Reserve the path without following an existing file or symlink.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&history)
            .map_err(|_| error("session_io", "Cannot create agent history file"))?;
        spawn(&command, &root, &url, &token, id, &history, sessions.wake.clone())
    })();
    let process = match result {
        Ok(p) => p,
        Err(e) => {
            sessions.tokens.revoke(id);
            return Err(e);
        }
    };
    let secrets = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, value)| {
            !value.is_empty() && {
                let key = key.to_ascii_lowercase();
                ["token", "secret", "password", "api_key"]
                    .iter()
                    .any(|part| key.contains(part))
            }
        })
        .map(|(_, value)| value)
        .collect();
    let mut session = Session {
        id,
        name: command.name,
        mode,
        token,
        process: Some(process),
        messages: VecDeque::new(),
        plan: None,
        history,
        root,
        history_error: false,
        history_dirty: false,
        secrets,
        output_backlog: false,
        staged: Vec::new(),
        next_staged: 1,
    };
    if let Err(error) = session.send(json!({"kind":"mode", "mode":mode})) {
        sessions.tokens.revoke(id);
        return Err(error);
    }
    session.record("system", "Agent started");
    session.flush_history();
    let answer = session.snapshot();
    sessions.sessions.insert(id, session);
    Ok(answer)
}

fn parse_mode(value: &Value) -> Result<Mode, RpcError> {
    serde_json::from_value(value.clone())
        .map_err(|_| error("session_bad_mode", "Mode is ask, propose, or act"))
}

/// Runs an Agent panel operation; also the registered protocol handler.
pub fn agent_sessions(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or("list");
    // A hosted child or scoped tool cannot raise its own permissions or approve its own plan.
    if action != "list" && regia_agent::scope::current_caller(world).is_some() {
        return Err(denied(
            "session_owner_only",
            "Hosted agents cannot control agent sessions",
        ));
    }
    if action == "list" {
        let configured: Vec<String> = commands(world)?.into_iter().map(|c| c.name).collect();
        let rows: Vec<Value> = world
            .get_resource::<AgentSessions>()
            .map(|s| s.sessions.values().map(Session::snapshot).collect())
            .unwrap_or_default();
        return Ok(json!({"commands":configured, "sessions":rows}));
    }
    if action == "start" {
        let name = params
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| error("session_bad_commands", "command is a configured name"))?;
        let mode = params
            .get("mode")
            .map(parse_mode)
            .transpose()?
            .unwrap_or_default();
        return start(world, name, mode);
    }
    let id = params
        .get("session")
        .and_then(Value::as_u64)
        .ok_or_else(|| error("session_not_found", "session is a session id"))?;
    if action == "staged" {
        return decide_staged(world, id, params);
    }
    let mut sessions = world.get_resource_or_insert_with::<AgentSessions>(Default::default);
    let tokens = sessions.tokens.clone();
    let session = sessions
        .sessions
        .get_mut(&id)
        .ok_or_else(|| error("session_not_found", "No such agent session"))?;
    match action {
        "stop" => {
            session.record("system", "Agent stopped");
            tokens.revoke(id);
            session.stop();
        }
        "send" => {
            if let Some(approve) = params.get("approve_plan") {
                let approve = approve
                    .as_bool()
                    .ok_or_else(|| error("session_bad_plan", "approve_plan is a boolean"))?;
                let plan = session
                    .plan
                    .as_ref()
                    .ok_or_else(|| error("session_bad_plan", "No plan is waiting"))?;
                if plan.approved {
                    return Err(error("session_bad_plan", "This plan was already approved"));
                }
                session.send(json!({"kind":"plan_decision", "approved":approve, "mode":session.mode}))?;
                if approve && let Some(plan) = &mut session.plan {
                    plan.approved = true;
                }
                // Rejection stays locked until a replacement plan is approved.
                session.record("user", if approve { "Plan approved" } else { "Plan rejected" });
            } else {
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty() && t.len() <= MAX_TEXT)
                    .ok_or_else(|| error("session_bad_message", "text must hold 1..8192 bytes"))?;
                session.send(json!({"kind":"message", "text":text, "mode":session.mode}))?;
                session.record("user", text);
            }
        }
        "mode" => {
            let mode = params
                .get("mode")
                .ok_or_else(|| error("session_bad_mode", "mode is required"))
                .and_then(parse_mode)?;
            session.send(json!({"kind":"mode", "mode":mode}))?;
            session.mode = mode;
            // Switching modes never approves a pending plan.
            session.record("system", mode.label());
        }
        _ => {
            return Err(error(
                "session_bad_action",
                "action is list, start, stop, send, mode, or staged",
            ));
        }
    }
    session.flush_history();
    Ok(session.snapshot())
}

/// Whether a Propose session's call to `name` is staged instead of refused: a synchronous
/// `editor.*` method beyond the Read class that can run inside one undo group.
fn stageable(world: &World, name: &str) -> bool {
    name.starts_with("editor.")
        && name != "editor.agent_sessions"
        && !UNSTAGEABLE.contains(&name)
        && world
            .get_resource::<regia_agent::extension::AgentExtension>()
            .and_then(|ext| ext.method(name))
            .is_some_and(|(method, _)| {
                matches!(method.permission, Permission::EditScene | Permission::Assets)
            })
}

/// Parameter keys whose values name entities (ids) or project files (paths) in editor methods.
const ENTITY_KEYS: &[&str] = &[
    "entity", "entities", "parent", "target", "targets", "root", "roots", "source", "child",
];
const PATH_KEYS: &[&str] = &["path", "paths", "asset", "assets", "file", "level", "prefab"];

/// What a staged call would touch, from its schema and parameters, without running it.
fn preview(world: &World, method: &MethodSchema, params: &Value) -> Value {
    let summary = method
        .description
        .split(". ")
        .next()
        .unwrap_or(method.description)
        .trim_end_matches('.');
    let mut touches = Vec::new();
    let entity = |bits: u64| match crate::command::resolve_entity(world, bits) {
        Some(entity) => format!(
            "entity {bits} \"{}\"",
            crate::outliner::display_name(world, entity)
        ),
        None => format!("entity {bits} (missing)"),
    };
    if let Some(object) = params.as_object() {
        for (key, value) in object {
            let values: Vec<&Value> = match value {
                Value::Array(items) => items.iter().collect(),
                other => vec![other],
            };
            if ENTITY_KEYS.contains(&key.as_str()) {
                touches.extend(values.iter().filter_map(|v| v.as_u64()).map(&entity));
            } else if PATH_KEYS.contains(&key.as_str()) {
                touches.extend(
                    values
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|path| format!("file {path}")),
                );
            }
        }
    }
    let creates = ["spawn", "duplicate", "paste", "create_entity"]
        .iter()
        .any(|verb| method.name.contains(verb));
    if creates {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("unnamed");
        touches.insert(0, format!("new entity \"{name}\""));
    }
    let takes_entities = method.params.iter().any(|p| p.name == "entities");
    if touches.is_empty() {
        touches.push(if takes_entities && params.get("entities").is_none() {
            "the current selection".to_string()
        } else {
            "editor state".to_string()
        });
    }
    json!({"summary": summary, "permission": method.permission.name(), "touches": touches})
}

/// `staged`: the owner approves (runs) or rejects the session's staged calls, all of them or
/// the `items` named by id. Approved calls run in staging order as one undo entry; if one
/// fails, the calls before it are reverted and nothing stays applied.
fn decide_staged(world: &mut World, id: u64, params: &Value) -> Result<Value, RpcError> {
    let approve = params
        .get("approve")
        .and_then(Value::as_bool)
        .ok_or_else(|| error("session_bad_staged", "approve is a boolean"))?;
    let items: Option<Vec<u64>> = match params.get("items") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| {
                    item.as_u64()
                        .ok_or_else(|| error("session_bad_staged", "items holds staged call ids"))
                })
                .collect::<Result<_, _>>()?,
        ),
        Some(_) => return Err(error("session_bad_staged", "items holds staged call ids")),
    };
    let chosen = {
        let mut sessions = world.get_resource_or_insert_with::<AgentSessions>(Default::default);
        let session = sessions
            .sessions
            .get_mut(&id)
            .ok_or_else(|| error("session_not_found", "No such agent session"))?;
        if session.staged.is_empty() {
            return Err(error("session_bad_staged", "No calls are staged"));
        }
        if let Some(items) = &items
            && (items.is_empty()
                || items
                    .iter()
                    .any(|item| !session.staged.iter().any(|s| s.id == *item)))
        {
            return Err(error(
                "session_bad_staged",
                "items names a call that is not staged",
            ));
        }
        let (chosen, kept): (Vec<Staged>, Vec<Staged>) = std::mem::take(&mut session.staged)
            .into_iter()
            .partition(|staged| items.as_ref().is_none_or(|items| items.contains(&staged.id)));
        session.staged = kept;
        chosen
    };
    let ids: Vec<u64> = chosen.iter().map(|s| s.id).collect();
    let outcome = if approve {
        run_staged(world, &chosen)
    } else {
        Ok(())
    };
    let mut sessions = world.resource_mut::<AgentSessions>();
    let Some(session) = sessions.sessions.get_mut(&id) else {
        return Err(error("session_not_found", "No such agent session"));
    };
    if let Err(StagedRun::Busy) = outcome {
        // Nothing ran: the chosen calls go back, still in staging order.
        session.staged.extend(chosen);
        session.staged.sort_by_key(|staged| staged.id);
        return Err(error(
            "session_staged_busy",
            "Another undo transaction is open; approve again after it ends",
        ));
    }
    let count = ids.len();
    let (record, failure) = match (&outcome, approve) {
        (Ok(()), true) => (format!("Approved {count} staged calls"), None),
        (Ok(()), false) => (format!("Rejected {count} staged calls"), None),
        (Err(StagedRun::Busy), _) => (String::new(), None),
        (Err(StagedRun::Failed(index, cause)), _) => {
            let method = chosen.get(*index).map_or("", |s| s.method.as_str());
            (
                format!(
                    "Approved {count} staged calls; {method} failed ({}) and none were applied",
                    cause.message
                ),
                Some(error(
                    "session_staged_failed",
                    &format!(
                        "Staged call {} ({method}) failed: {}. None of the approved calls were applied",
                        ids.get(*index).copied().unwrap_or_default(),
                        cause.message
                    ),
                )),
            )
        }
    };
    session.record("user", &record);
    // The child learns the decision; a stopped child is told nothing (its queue was cleared).
    if session.process.is_some() {
        let _ = session.send(json!({"kind":"staged_decision", "mode":session.mode,
            "approved": approve && outcome.is_ok(), "items": ids}));
    }
    session.flush_history();
    match failure {
        Some(failure) => Err(failure),
        None => Ok(session.snapshot()),
    }
}

/// Why approved staged calls did not all run.
enum StagedRun {
    /// Another undo transaction is open; nothing ran.
    Busy,
    /// The call at this index failed with this error; the calls before it were reverted.
    Failed(usize, RpcError),
}

/// Runs approved staged calls in order inside one undo transaction (the owner's, not an agent
/// transaction, so every command they push joins it). On a failure it aborts the transaction,
/// reverting the calls before it, and returns the failing index and its error.
fn run_staged(world: &mut World, calls: &[Staged]) -> Result<(), StagedRun> {
    let _span = tracing::trace_span!("agent_sessions_run_staged", calls = calls.len()).entered();
    crate::procgen::undo::drain(world);
    let label = format!("Agent proposal ({} calls)", calls.len());
    world
        .get_resource_or_insert_with::<crate::command::UndoStack>(crate::command::UndoStack::new)
        .transaction_begin(label)
        .map_err(|_| StagedRun::Busy)?;
    for (index, call) in calls.iter().enumerate() {
        let (reply, answer) = crossbeam_channel::bounded(1);
        regia_agent::process_hosted_request(
            world,
            regia_agent::scope::ScopedInbound {
                request: regia_agent::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(call.id)),
                    method: call.method.clone(),
                    params: call.params.clone(),
                },
                reply,
                scope: Arc::clone(&call.scope),
            },
            Some(&[Permission::Read, Permission::EditScene, Permission::Assets]),
            // The call was stageable when staged; recheck the name-level rules (the world is
            // borrowed by the dispatcher here). Project permissions still apply as usual.
            |method| {
                if method.name.starts_with("editor.")
                    && method.name != "editor.agent_sessions"
                    && !UNSTAGEABLE.contains(&method.name)
                    && matches!(method.permission, Permission::EditScene | Permission::Assets)
                {
                    Ok(())
                } else {
                    Err(denied(
                        "session_propose_live_unsupported",
                        "This call cannot run as an approved proposal",
                    ))
                }
            },
        );
        let failure = match answer.try_recv() {
            Ok(regia_agent::protocol::Response { error: None, .. }) => None,
            Ok(regia_agent::protocol::Response {
                error: Some(cause), ..
            }) => Some(cause),
            Err(_) => Some(error("session_staged_failed", "The call did not finish")),
        };
        if let Some(cause) = failure {
            crate::procgen::undo::drain(world);
            let mut stack = world
                .remove_resource::<crate::command::UndoStack>()
                .unwrap_or_default();
            let _ = stack.transaction_abort(world);
            world.insert_resource(stack);
            // Drop reverted entities from the selection, as editor.transaction_abort does.
            if let Some(mut selection) = world.remove_resource::<crate::selection::Selection>() {
                selection.retain_alive(world);
                world.insert_resource(selection);
            }
            return Err(StagedRun::Failed(index, cause));
        }
    }
    crate::procgen::undo::drain(world);
    if let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        let _ = stack.transaction_commit();
    }
    Ok(())
}

fn guard(mode: Mode, plan_pending: bool, stage: bool, method: &MethodSchema) -> Result<(), RpcError> {
    if method.name == "editor.agent_sessions"
        || (mode == Mode::Propose && method.permission == Permission::Review)
    {
        return Err(denied(
            "session_owner_only",
            "Hosted agents cannot approve work or control sessions",
        ));
    }
    if method.permission == Permission::Read {
        return Ok(());
    }
    if mode == Mode::Ask {
        return Err(denied("F0012", "Ask only grants the Read permission class"));
    }
    if plan_pending {
        return Err(denied(
            "session_plan_pending",
            "The owner must approve the current plan first",
        ));
    }
    if mode == Mode::Propose && method.name != "review.propose" && method.name != "review.reject" {
        // Past every permission and plan check: the tick stages the call instead of running it.
        if stage {
            return Err(RpcError::new(
                PERMISSION_DENIED,
                STAGE_SIGNAL,
                "staged for the owner's approval",
                "",
            ));
        }
        return Err(denied(
            "session_propose_live_unsupported",
            "Propose stages live editor.* edits and file changes through review.propose; this call can be neither",
        ));
    }
    Ok(())
}

/// Dispatches a Propose session's stageable call through the ordinary permission path; when
/// the guard signals staging, queues it with a preview and answers `{staged, staged_id, ...}`.
/// Any other answer (a permission or plan refusal) goes to the caller unchanged.
fn dispatch_or_stage(world: &mut World, inbound: regia_agent::scope::ScopedInbound, plan_pending: bool) {
    let regia_agent::scope::ScopedInbound {
        request,
        reply,
        scope,
    } = inbound;
    let (inner, answer) = crossbeam_channel::bounded(1);
    regia_agent::process_hosted_request(
        world,
        regia_agent::scope::ScopedInbound {
            request: request.clone(),
            reply: inner,
            scope: Arc::clone(&scope),
        },
        Some(&[Permission::Read, Permission::EditScene, Permission::Assets]),
        |method| guard(Mode::Propose, plan_pending, true, method),
    );
    // The guard refuses before any handler runs, so the answer is already here.
    let Ok(response) = answer.try_recv() else {
        return;
    };
    let signalled = response
        .error
        .as_ref()
        .and_then(|e| e.data.as_ref())
        .is_some_and(|data| data["code"] == STAGE_SIGNAL);
    if !signalled {
        let _ = reply.send(response);
        return;
    }
    let id = response.id.clone();
    let method = world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .and_then(|ext| ext.method_schema(&request.method).cloned());
    let preview = method.map(|method| preview(world, &method, &request.params));
    let mut sessions = world.resource_mut::<AgentSessions>();
    let Some(session) = sessions.sessions.get_mut(&scope.id) else {
        let _ = reply.send(regia_agent::protocol::Response::err(
            id,
            regia_agent::scope::revoked_error(&scope.tool),
        ));
        return;
    };
    if session.staged.len() >= MAX_STAGED {
        let _ = reply.send(regia_agent::protocol::Response::err(
            id,
            denied(
                "session_staged_full",
                "32 calls are already staged; wait for the owner to decide them",
            ),
        ));
        return;
    }
    let staged_id = session.next_staged;
    session.next_staged += 1;
    let preview = preview.unwrap_or_else(|| json!({"summary":"", "touches":["editor state"]}));
    let preview: Value =
        serde_json::from_str(&session.redact(&preview.to_string())).unwrap_or_else(|_| json!({}));
    let params_text = session.redact(&request.params.to_string());
    session.record(
        "system",
        &format!(
            "Staged {}: {}",
            request.method,
            preview["touches"]
                .as_array()
                .map(|t| t.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
                .unwrap_or_default()
        ),
    );
    let answer = json!({"staged": true, "staged_id": staged_id, "preview": preview,
        "pending": session.staged.len() + 1});
    session.staged.push(Staged {
        id: staged_id,
        method: request.method,
        params: request.params,
        scope,
        preview,
        params_text,
    });
    session.flush_history();
    let _ = reply.send(regia_agent::protocol::Response::ok(id, answer));
}

/// Drains bounded child output and private protocol requests, including when the panel is closed.
/// No file IO or repaint occurs on an idle tick.
pub fn tick(world: &mut World) {
    if !world.contains_resource::<AgentSessions>() {
        return;
    }
    let _span = tracing::trace_span!("agent_sessions_tick").entered();
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    let mut wake_next_frame = false;
    // Read stdout before calls, so an emitted Plan closes the mutation gate before dispatch. A
    // drain that stops at its bound marks the session, and its calls wait for the rest (below).
    {
        let mut sessions = world.resource_mut::<AgentSessions>();
        let tokens = sessions.tokens.clone();
        for session in sessions.sessions.values_mut() {
            if root.as_ref() != Some(&session.root) && session.process.is_some() {
                tokens.revoke(session.id);
                session.record("system", "Agent stopped: project changed");
                session.stop();
                continue;
            }
            if session
                .process
                .as_ref()
                .is_some_and(|p| p.failed.load(Ordering::Acquire))
            {
                tokens.revoke(session.id);
                session.history_error = true;
                session.record("system", "Agent stopped: output queue overflow");
                session.stop();
                continue;
            }
            let mut drained = 0;
            for _ in 0..8 {
                let event = session
                    .process
                    .as_ref()
                    .and_then(|p| p.events.lock().ok()?.try_recv().ok());
                drained += usize::from(event.is_some());
                match event {
                    Some(ChildEvent::Output(Output::Message { text })) if text.len() <= MAX_TEXT => {
                        session.record("agent", &text)
                    }
                    Some(ChildEvent::Output(Output::Plan { steps }))
                        if !steps.is_empty()
                            && steps.len() <= MAX_STEPS
                            && steps.iter().all(|s| !s.trim().is_empty() && s.len() <= 512) =>
                    {
                        let steps: Vec<String> = steps.iter().map(|s| session.redact(s)).collect();
                        session.record("plan", &steps.join("\n"));
                        let revision = session
                            .plan
                            .as_ref()
                            .map_or(1, |plan| plan.revision.saturating_add(1));
                        session.plan = Some(Plan {
                            steps,
                            approved: false,
                            revision,
                        });
                        if session
                            .send(json!({"kind":"plan_pending", "mode":session.mode}))
                            .is_err()
                        {
                            tokens.revoke(session.id);
                            session.stop();
                        }
                    }
                    Some(ChildEvent::End | ChildEvent::Fault) => {
                        tokens.revoke(session.id);
                        session.record("system", "Agent ended");
                        session.stop();
                        break;
                    }
                    Some(_) => {
                        tokens.revoke(session.id);
                        session.record("system", "Agent stopped: invalid message or plan limits");
                        session.stop();
                        break;
                    }
                    None => break,
                }
            }
            // A full drain may have left a plan unread; hold this session's calls until it is read.
            session.output_backlog = drained == 8 && session.process.is_some();
            wake_next_frame |= session.output_backlog;
            session.flush_history();
        }
    }
    let mut dispatched = 0;
    let mut holding = false;
    for _ in 0..32 {
        let request = {
            let mut sessions = world.resource_mut::<AgentSessions>();
            match sessions.held.take() {
                Some(held) => Some(held),
                None => sessions.server.as_ref().and_then(|s| s.inbox.try_recv().ok()),
            }
        };
        let Some(inbound) = request else {
            break;
        };
        let state = {
            let sessions = world.resource::<AgentSessions>();
            sessions
                .sessions
                .get(&inbound.scope.id)
                .filter(|_| sessions.tokens.is_live(inbound.scope.id))
                .map(|s| {
                    (
                        s.mode,
                        s.plan.as_ref().is_some_and(|p| !p.approved),
                        s.output_backlog,
                    )
                })
        };
        // Unread stdout may hold a plan emitted before this call: wait for the next drain. Later
        // requests stay in the inbox behind it, so the order is kept.
        if state.is_some_and(|(_, _, backlog)| backlog) {
            world.resource_mut::<AgentSessions>().held = Some(inbound);
            holding = true;
            break;
        }
        dispatched += 1;
        if let Some((Mode::Propose, plan_pending, _)) = state
            && stageable(world, &inbound.request.method)
        {
            dispatch_or_stage(world, inbound, plan_pending);
        } else if let Some((mode, plan_pending, _)) = state {
            let permissions: Option<&[Permission]> = match mode {
                Mode::Ask => Some(&[Permission::Read]),
                Mode::Propose => Some(&[Permission::Read, Permission::EditScene, Permission::Assets]),
                Mode::Act => None,
            };
            regia_agent::process_hosted_request(world, inbound, permissions, |method| {
                guard(mode, plan_pending, false, method)
            });
        } else {
            let id = inbound.request.id.unwrap_or(Value::Null);
            let _ = inbound.reply.send(regia_agent::protocol::Response::err(
                id,
                regia_agent::scope::revoked_error(&inbound.scope.tool),
            ));
        }
    }
    // Event-loop wakes can coalesce. Exhausting either bounded drain must arrange
    // another frame, even if no new child output or request arrives afterward.
    if (wake_next_frame || holding || dispatched == 32)
        && let Some(wake) = &world.resource::<AgentSessions>().wake
    {
        wake();
    }
}

/// Schema and handler for the panel's complete session operation surface.
pub fn methods() -> Vec<(MethodSchema, crate::viewport_agent::Handler)> {
    vec![(
        MethodSchema {
            name: "editor.agent_sessions",
            tool: "editor_agent_sessions",
            description: "List, start, stop, message or change a configured hosted agent session; approve plans through send; approve or reject a Propose session's staged editor calls through staged.",
            params: &[
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: false,
                    description: "list (default), start, stop, send, mode, or staged.",
                },
                ParamSchema {
                    name: "command",
                    kind: "string",
                    required: false,
                    description: "Configured command name for start.",
                },
                ParamSchema {
                    name: "session",
                    kind: "integer",
                    required: false,
                    description: "Session id for stop, send, mode or staged.",
                },
                ParamSchema {
                    name: "mode",
                    kind: "string",
                    required: false,
                    description: "ask (default), propose or act.",
                },
                ParamSchema {
                    name: "text",
                    kind: "string",
                    required: false,
                    description: "Message for send, at most 8192 bytes.",
                },
                ParamSchema {
                    name: "approve_plan",
                    kind: "boolean",
                    required: false,
                    description: "For send: approve or reject the waiting plan instead of sending text.",
                },
                ParamSchema {
                    name: "approve",
                    kind: "boolean",
                    required: false,
                    description: "For staged: true runs the chosen staged calls in order as one undo entry; false discards them.",
                },
                ParamSchema {
                    name: "items",
                    kind: "array",
                    required: false,
                    description: "For staged: staged call ids to decide (default: all of them).",
                },
            ],
            result: "{commands,sessions} for list; session snapshot otherwise, with staged: [{id, method, params, preview}]. Tokens and command arguments are never returned.",
            mutating: true,
            permission: Permission::Tools,
        },
        agent_sessions,
    )]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::editor_prefs::{EditorPreferences, PrefValue};
    use crate::selection::Selection;
    use regia_agent::protocol::{Request, Response};
    use regia_app::prelude::*;
    use std::time::Duration;

    const HANG_GUARD: Duration = Duration::from_secs(90);

    pub(crate) struct Harness {
        pub(crate) app: App,
        events: mpsc::Receiver<()>,
        root: PathBuf,
    }

    impl Harness {
        pub(crate) fn new() -> Self {
            let root = regia_test_utils::unique_temp_dir("agent-sessions");
            std::fs::create_dir_all(&root).expect("project");
            let mut app = App::new();
            app.add_plugins(MinimalPlugins);
            app.init_resource::<Selection>();
            app.insert_resource(crate::agent_ext::EditorProject {
                root: Some(root.clone()),
            });
            app.insert_resource(regia_agent::AgentSettings {
                project_dir: Some(root.clone()),
            });
            app.add_plugins(regia_scene::ScenePlugin);
            app.add_plugins(regia_agent::AgentPlugin);
            app.finish_plugins();
            app.update();
            let mut permissions = regia_agent::AgentPermissions::default();
            permissions.config.permissions.insert("tools".into());
            permissions.config.rate_limit_per_second = 10_000;
            app.insert_resource(permissions);
            crate::agent_ext::install(app.world_mut());
            let (tx, events) = mpsc::channel();
            app.insert_resource(AgentSessions {
                sessions: BTreeMap::new(),
                tokens: ToolTokens::default(),
                server: None,
                wake: Some(Arc::new(move || {
                    let _ = tx.send(());
                })),
                held: None,
            });
            let mut prefs = EditorPreferences::default();
            let executable = std::env::current_exe().expect("test binary");
            prefs.set("agents.commands", &PrefValue::Text(json!([{
                "name":"Fixture", "command":[executable, "--exact", "agent_sessions::tests::fake_agent", "--ignored", "--nocapture", "--test-threads=1"]
            }]).to_string())).expect("preference");
            app.insert_resource(prefs);
            Self { app, events, root }
        }
        pub(crate) fn call(&mut self, params: Value) -> Value {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (reply, answer) = crossbeam_channel::bounded(1);
            tx.send(regia_agent::server::Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: "editor.agent_sessions".into(),
                    params,
                },
                reply,
            })
            .expect("request");
            self.app.world_mut().insert_resource(regia_agent::AgentInbox(rx));
            self.app.update();
            let Response { result, error, .. } = answer.try_recv().expect("dispatcher answered");
            assert!(error.is_none(), "{error:?}");
            result.expect("result")
        }
        pub(crate) fn start(&mut self, mode: Mode) -> u64 {
            let id = self.call(json!({"action":"start", "command":"Fixture", "mode":mode}))["id"]
                .as_u64()
                .expect("session id");
            self.wait_for(id, "ready");
            id
        }
        pub(crate) fn wait_for(&mut self, id: u64, text: &str) {
            loop {
                tick(self.app.world_mut());
                let sessions = self.app.world().resource::<AgentSessions>();
                let session = sessions.sessions.get(&id).expect("session");
                if session.messages.iter().any(|message| message.text.contains(text)) {
                    return;
                }
                assert!(
                    session.process.is_some(),
                    "agent stopped while waiting for {text}: {}",
                    session.snapshot()
                );
                self.events
                    .recv_timeout(HANG_GUARD)
                    .expect("child or socket wake (hang guard)");
            }
        }
        fn send(&mut self, id: u64, text: Value) {
            self.call(json!({"action":"send", "session":id, "text":text.to_string()}));
        }
        /// Has the fixture child call `method` and returns its answer line (`reply:…` or
        /// `refused:…`), the first agent answer after the request.
        pub(crate) fn child_call(&mut self, id: u64, method: &str, params: Value) -> String {
            let before = self.app.world().resource::<AgentSessions>().sessions[&id]
                .messages
                .len();
            self.send(id, json!({"method":method, "params":params}));
            loop {
                tick(self.app.world_mut());
                let sessions = self.app.world().resource::<AgentSessions>();
                let session = sessions.sessions.get(&id).expect("session");
                if let Some(answer) = session.messages.iter().skip(before).find(|m| {
                    m.role == "agent" && (m.text.starts_with("reply:") || m.text.starts_with("refused:"))
                }) {
                    return answer.text.clone();
                }
                assert!(session.process.is_some(), "agent stopped: {}", session.snapshot());
                self.events
                    .recv_timeout(HANG_GUARD)
                    .expect("child or socket wake (hang guard)");
            }
        }
    }

    /// The level as a scene file: every entity with a guid, its name, hierarchy and components.
    pub(crate) fn scene(world: &mut World) -> regia_scene::prelude::SceneFile {
        regia_scene::prelude::snapshot_scene(world, &regia_scene::spawn::SnapshotFilter::default())
            .expect("scene snapshot")
    }

    fn undo_depth(world: &World) -> usize {
        world
            .get_resource::<crate::command::UndoStack>()
            .map_or(0, crate::command::UndoStack::undo_depth)
    }

    pub(crate) fn named(world: &mut World, name: &str) -> usize {
        scene(world)
            .entities
            .iter()
            .filter(|e| e.name.as_deref() == Some(name))
            .count()
    }
    impl Drop for Harness {
        fn drop(&mut self) {
            self.app.world_mut().remove_resource::<AgentSessions>();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn emit(value: Value) {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{value}").expect("fixture output");
        out.flush().expect("flush");
    }

    /// A Rust-only stand-in for a user-supplied agent executable. The harness starts this test
    /// as a child, so its stdin/stdout, environment and protocol client are all real.
    #[test]
    #[ignore = "child-process fixture, launched by the session tests"]
    fn fake_agent() {
        if std::env::var_os("REGIA_AGENT_LINE_PROTOCOL").is_none() {
            return;
        }
        let (mut client, _) =
            regia_agent::client::Client::connect_tool_from_env().expect("private session connection");
        // libtest prints its test name before calling us; begin on a fresh protocol line.
        println!();
        emit(json!({"kind":"message", "text":"ready"}));
        let mut descendants = Vec::new();
        for line in std::io::stdin().lock().lines() {
            let line = line.expect("input");
            let value: Value = serde_json::from_str(&line).expect("JSON input");
            if value["kind"] != "message" {
                continue;
            }
            let text = value["text"].as_str().expect("message text");
            let request: Value = serde_json::from_str(text).expect("fixture request");
            if request["fixture"] == "plan" {
                let mut steps = vec![
                    "Select the test entity".to_owned(),
                    "Inspect the selection".to_owned(),
                ];
                let count = request["steps"].as_u64().unwrap_or(2).min(MAX_STEPS as u64);
                steps.extend((2..count).map(|step| format!("Review step {}", step + 1)));
                emit(json!({"kind":"plan", "steps":steps}));
                continue;
            }
            if request["fixture"] == "cwd" {
                emit(
                    json!({"kind":"message", "text":format!("cwd:{}", std::env::current_dir().expect("cwd").display())}),
                );
                continue;
            }
            if request["fixture"] == "oversized" {
                println!("{}", "x".repeat(MAX_LINE + 1));
                std::io::stdout().flush().expect("flush oversized line");
                continue;
            }
            if request["fixture"] == "invalid_plan" {
                emit(json!({"kind":"plan", "steps":"not a checklist"}));
                continue;
            }
            if request["fixture"] == "descendant" {
                let descendant = ProcessCommand::new(std::env::current_exe().expect("test binary"))
                    .args([
                        "--exact",
                        "agent_sessions::tests::fake_descendant",
                        "--ignored",
                        "--nocapture",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("descendant");
                emit(json!({"kind":"message", "text":format!("descendant:{}", descendant.id())}));
                // Keep the child handle alive until the parent is killed; the session owns the group.
                descendants.push(descendant);
                continue;
            }
            if request["fixture"] == "credential" {
                emit(
                    json!({"kind":"message", "text":std::env::var(regia_agent::scope::TOKEN_ENV).expect("token")}),
                );
                continue;
            }
            let method = request["method"].as_str().expect("method");
            let result = client.call(method, request["params"].clone());
            emit(
                json!({"kind":"message", "text": match result { Ok(value) => format!("reply:{value}"), Err(error) => format!("refused:{error}") }}),
            );
        }
        for mut child in descendants {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    #[test]
    fn agent_sessions_fake_child_selects_in_act_and_is_refused_in_ask() {
        let mut h = Harness::new();
        let entity = h.app.world_mut().spawn_empty().id();
        let id = h.start(Mode::Ask);
        h.send(
            id,
            json!({"method":"editor.select", "params":{"entities":[entity.to_bits()]}}),
        );
        h.wait_for(id, "F0012");
        assert!(h.app.world().resource::<Selection>().is_empty());
        h.call(json!({"action":"mode", "session":id, "mode":"act"}));
        h.send(
            id,
            json!({"method":"editor.select", "params":{"entities":[entity.to_bits()]}}),
        );
        h.wait_for(id, "reply:");
        assert!(!h.app.world().resource::<Selection>().is_empty());
        let session = &h.app.world().resource::<AgentSessions>().sessions[&id];
        assert!(
            session
                .messages
                .iter()
                .any(|m| m.role == "agent" && m.mode == Mode::Ask && m.text.contains("F0012"))
        );
        assert!(
            session
                .messages
                .iter()
                .any(|m| m.role == "agent" && m.mode == Mode::Act && m.text.starts_with("reply:"))
        );
    }

    #[test]
    fn agent_sessions_plan_requires_owner_approval_even_after_mode_change() {
        let mut h = Harness::new();
        let entity = h.app.world_mut().spawn_empty().id();
        let id = h.start(Mode::Act);
        h.send(id, json!({"fixture":"plan"}));
        h.wait_for(id, "Select the test entity");
        h.call(json!({"action":"mode", "session":id, "mode":"ask"}));
        h.call(json!({"action":"mode", "session":id, "mode":"act"}));
        h.send(
            id,
            json!({"method":"editor.select", "params":{"entities":[entity.to_bits()]}}),
        );
        h.wait_for(id, "session_plan_pending");
        assert!(h.app.world().resource::<Selection>().is_empty());
        h.send(id, json!({"method":"editor.agent_sessions", "params":{"action":"send", "session":id, "approve_plan":true}}));
        h.wait_for(id, "session_owner_only");
        h.call(json!({"action":"send", "session":id, "approve_plan":true}));
        h.send(
            id,
            json!({"method":"editor.select", "params":{"entities":[entity.to_bits()]}}),
        );
        h.wait_for(id, "reply:");
        assert!(!h.app.world().resource::<Selection>().is_empty());
    }

    #[test]
    fn agent_sessions_propose_stages_files_and_refuses_live_edits() {
        let mut h = Harness::new();
        let id = h.start(Mode::Propose);
        h.send(id, json!({"method":"review.propose", "params":{"reason":"Test change", "changes":[{"path":"note.txt", "text":"proposed"}]}}));
        h.wait_for(id, "reply:");
        assert!(!h.root.join("note.txt").exists());
        let ids: Vec<u64> = regia_agent::review::queue(h.app.world_mut())
            .pending()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids.len(), 1);
        // P-369 (ADR-8375): a live editor edit is staged, never applied, until the owner decides;
        // calls that cannot be staged (outside editor.*, or history control) stay refused.
        let entity = h.app.world_mut().spawn_empty().id();
        let select = h.child_call(id, "editor.select", json!({"entities":[entity.to_bits()]}));
        assert!(select.contains("\"staged\":true"), "{select}");
        assert!(h.app.world().resource::<Selection>().is_empty());
        let undo = h.child_call(id, "editor.undo", json!({}));
        assert!(undo.contains("session_propose_live_unsupported"), "{undo}");
        crate::agent_changes::decide(h.app.world_mut(), &json!({"ids":ids}), true).expect("owner accepts");
        assert_eq!(
            std::fs::read_to_string(h.root.join("note.txt")).expect("accepted file"),
            "proposed"
        );
    }

    #[test]
    fn agent_sessions_cwd_private_history_stop_and_revocation() {
        let mut h = Harness::new();
        let id = h.start(Mode::Act);
        h.send(id, json!({"fixture":"cwd"}));
        h.wait_for(id, &format!("cwd:{}", h.root.display()));
        h.send(id, json!({"fixture":"credential"}));
        h.wait_for(id, "[redacted]");
        let (token, tokens, history) = {
            let sessions = h.app.world().resource::<AgentSessions>();
            let session = &sessions.sessions[&id];
            (
                session.token.clone(),
                sessions.tokens.clone(),
                session.history.clone(),
            )
        };
        let stopped = h.call(json!({"action":"stop", "session":id}));
        assert_eq!(stopped["running"], false);
        assert!(!tokens.is_live(id));
        let history = std::fs::read_to_string(history).expect("history flushed on stop");
        assert!(history.contains("[redacted]"));
        assert!(!history.contains(&token));
        assert!(
            history
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
        // An idle tick does not create a new session, write history, or request a repaint.
        while h.events.try_recv().is_ok() {}
        tick(h.app.world_mut());
        assert!(h.events.try_recv().is_err());
    }

    #[test]
    fn agent_sessions_short_credentials_are_omitted_without_expanding_text() {
        let mut h = Harness::new();
        let id = h.start(Mode::Ask);
        h.app
            .world_mut()
            .resource_mut::<AgentSessions>()
            .sessions
            .get_mut(&id)
            .expect("session")
            .secrets
            .push("a".into());
        h.send(id, json!({"fixture":"cwd"}));
        h.wait_for(id, "[credential-bearing text omitted]");
        h.call(json!({"action":"stop", "session":id}));
        let sessions = h.app.world().resource::<AgentSessions>();
        let history = std::fs::read_to_string(&sessions.sessions[&id].history).expect("history");
        assert!(!history.contains(&format!("cwd:{}", h.root.display())));
    }

    #[test]
    fn agent_sessions_project_permissions_still_override_act() {
        let mut h = Harness::new();
        let id = h.start(Mode::Act);
        h.app
            .world_mut()
            .resource_mut::<regia_agent::AgentPermissions>()
            .config
            .permissions
            .remove("edit_scene");
        h.send(id, json!({"method":"editor.select", "params":{"entities":[]}}));
        h.wait_for(id, "F0012");
    }

    #[test]
    fn agent_sessions_credentials_are_refused_before_preferences_can_persist() {
        let mut prefs = EditorPreferences::default();
        assert!(
            prefs
                .set(
                    "agents.commands",
                    &PrefValue::Text(r#"[{"name":"Bad","command":["agent","--api-key=credential"]}]"#.into())
                )
                .is_err()
        );
        assert_eq!(prefs.text("agents.commands"), "");
    }
    #[test]
    #[ignore = "grandchild fixture, launched by the process-tree test"]
    fn fake_descendant() {
        if std::env::var_os("REGIA_AGENT_LINE_PROTOCOL").is_none() {
            return;
        }
        loop {
            std::thread::park();
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn agent_sessions_stop_kills_the_descendant_tree() {
        let mut h = Harness::new();
        let id = h.start(Mode::Ask);
        h.send(id, json!({"fixture":"descendant"}));
        h.wait_for(id, "descendant:");
        let session = &h.app.world().resource::<AgentSessions>().sessions[&id];
        let pid = session
            .messages
            .iter()
            .find_map(|m| m.text.strip_prefix("descendant:"))
            .expect("pid")
            .parse::<u32>()
            .expect("numeric pid");
        let status = PathBuf::from(format!("/proc/{pid}/stat"));
        assert!(status.exists(), "grandchild alive before Stop");
        h.call(json!({"action":"stop", "session":id}));
        let (tx, rx) = mpsc::channel();
        let watcher = std::thread::spawn(move || {
            loop {
                let text = std::fs::read_to_string(&status).unwrap_or_default();
                let state = text
                    .rsplit_once(')')
                    .and_then(|(_, tail)| tail.split_whitespace().next());
                if text.is_empty() || state == Some("Z") || state == Some("X") {
                    break;
                }
                std::thread::yield_now();
            }
            let _ = tx.send(());
        });
        rx.recv_timeout(HANG_GUARD)
            .expect("grandchild terminated (hang guard, not a performance budget)");
        watcher.join().expect("watcher");
    }

    #[test]
    fn agent_sessions_invalid_output_revokes_and_stops() {
        for fixture in ["oversized", "invalid_plan"] {
            let mut h = Harness::new();
            let id = h.start(Mode::Act);
            h.send(id, json!({"fixture":fixture}));
            loop {
                tick(h.app.world_mut());
                let sessions = h.app.world().resource::<AgentSessions>();
                if sessions.sessions[&id].process.is_none() {
                    assert!(!sessions.tokens.is_live(id));
                    break;
                }
                h.events.recv_timeout(HANG_GUARD).expect("output failure wake");
            }
        }
    }

    #[test]
    fn agent_sessions_token_cannot_reconnect_after_stop() {
        let mut h = Harness::new();
        let id = h.start(Mode::Ask);
        let sessions = h.app.world().resource::<AgentSessions>();
        let token = sessions.sessions[&id].token.clone();
        let url = format!("ws://{}", sessions.server.as_ref().expect("listener").addr);
        h.call(json!({"action":"stop", "session":id}));
        assert!(regia_agent::client::Client::connect(&url, &token).is_err());
    }
    #[test]
    fn agent_sessions_output_overflow_is_a_fail_closed_signal() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let failed = AtomicBool::new(false);
        notify(
            &tx,
            ChildEvent::Output(Output::Message { text: "one".into() }),
            &None,
            &failed,
        );
        assert!(!failed.load(Ordering::Acquire));
        notify(
            &tx,
            ChildEvent::Output(Output::Plan {
                steps: vec!["Do not drop this gate".into()],
            }),
            &None,
            &failed,
        );
        assert!(
            failed.load(Ordering::Acquire),
            "losing a Plan must close the session, never silently allow edits"
        );
    }

    #[test]
    fn agent_sessions_bounded_output_drain_wakes_the_next_frame() {
        let mut h = Harness::new();
        let id = h.start(Mode::Ask);
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&wakes);
        let (tx, rx) = mpsc::sync_channel(16);
        for index in 0..9 {
            tx.send(ChildEvent::Output(Output::Message {
                text: format!("queued-{index}"),
            }))
            .expect("fixture queue");
        }
        {
            let mut sessions = h.app.world_mut().resource_mut::<AgentSessions>();
            sessions.wake = Some(Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            }));
            sessions
                .sessions
                .get_mut(&id)
                .expect("session")
                .process
                .as_mut()
                .expect("child")
                .events = Mutex::new(rx);
        }
        tick(h.app.world_mut());
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            1,
            "a bounded drain must schedule its remaining output"
        );
        tick(h.app.world_mut());
        assert!(
            h.app.world().resource::<AgentSessions>().sessions[&id]
                .messages
                .iter()
                .any(|message| message.text == "queued-8")
        );
        tick(h.app.world_mut());
        assert_eq!(wakes.load(Ordering::Relaxed), 1, "an idle tick does not repaint");
    }

    #[test]
    #[cfg(unix)]
    fn agent_sessions_non_utf8_history_path_is_safe_to_display() {
        use std::os::unix::ffi::OsStringExt as _;
        let mut h = Harness::new();
        let root = h
            .root
            .join(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
        std::fs::create_dir_all(&root).expect("project");
        h.app
            .world_mut()
            .resource_mut::<crate::agent_ext::EditorProject>()
            .root = Some(root.clone());
        h.app
            .world_mut()
            .resource_mut::<regia_agent::AgentSettings>()
            .project_dir = Some(root.clone());
        h.start(Mode::Ask);
        let reply = h.call(json!({"action":"list"}));
        assert!(
            reply["sessions"][0]["history"]
                .as_str()
                .expect("display path")
                .starts_with(root.to_string_lossy().as_ref())
        );
        assert!(root.join(".regia/agent_sessions").is_dir());
    }

    /// Review g15 item 8: a plan queued behind a full stdout drain must close the mutation gate
    /// before a call the child sent after it can run.
    #[test]
    fn agent_sessions_plan_behind_a_full_drain_closes_the_gate_before_dispatch() {
        let mut h = Harness::new();
        let entity = h.app.world_mut().spawn_empty().id();
        let id = h.start(Mode::Act);
        let (tx, rx) = mpsc::sync_channel(16);
        for index in 0..8 {
            tx.send(ChildEvent::Output(Output::Message {
                text: format!("chatter-{index}"),
            }))
            .expect("fixture queue");
        }
        tx.send(ChildEvent::Output(Output::Plan {
            steps: vec!["Wait for the owner".into()],
        }))
        .expect("fixture queue");
        let (requests, inbox) = crossbeam_channel::bounded(4);
        let (reply, answer) = crossbeam_channel::bounded(1);
        let mut scope = ToolScope::new(
            "hosted-agent:Fixture",
            [
                Permission::Read,
                Permission::EditScene,
                Permission::Assets,
                Permission::Play,
            ],
            vec!["*".into()],
        );
        scope.id = id;
        requests
            .send(regia_agent::scope::ScopedInbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(7)),
                    method: "editor.select".into(),
                    params: json!({"entities":[entity.to_bits()]}),
                },
                reply,
                scope: Arc::new(scope),
            })
            .expect("queued call");
        {
            let mut sessions = h.app.world_mut().resource_mut::<AgentSessions>();
            sessions.server.as_mut().expect("listener").inbox = inbox;
            sessions
                .sessions
                .get_mut(&id)
                .expect("session")
                .process
                .as_mut()
                .expect("child")
                .events = Mutex::new(rx);
        }
        tick(h.app.world_mut());
        assert!(
            h.app.world().resource::<Selection>().is_empty(),
            "a call queued behind an unread plan ran before the plan closed the gate"
        );
        tick(h.app.world_mut());
        let Response { error, .. } = answer
            .try_recv()
            .expect("held call answered after the plan was read");
        let error = error.expect("the plan refuses the held call");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("session_plan_pending")
        );
        assert!(h.app.world().resource::<Selection>().is_empty());
        assert!(h.app.world().resource::<AgentSessions>().held.is_none());
    }

    /// P-369 (ADR-8375): a Propose session's live `editor.spawn` is staged with a preview of
    /// what it would touch; the world, the undo history and the selection stay as they were.
    #[test]
    fn propose_mode_stages_a_spawn_without_changing_the_world() {
        let mut h = Harness::new();
        let id = h.start(Mode::Propose);
        let before = scene(h.app.world_mut());
        let depth = undo_depth(h.app.world());
        let answer = h.child_call(id, "editor.spawn", json!({"name":"Staged crate"}));
        assert!(
            answer.starts_with("reply:"),
            "the call was staged, not refused: {answer}"
        );
        let reply: Value = serde_json::from_str(answer.trim_start_matches("reply:")).expect("JSON reply");
        assert_eq!(reply["staged"], true, "{reply}");
        assert_eq!(scene(h.app.world_mut()), before, "staging changed the scene");
        assert_eq!(undo_depth(h.app.world()), depth, "staging pushed an undo entry");
        let state = h.call(json!({"action":"list"}));
        let staged = state["sessions"][0]["staged"].as_array().expect("staged list");
        assert_eq!(staged.len(), 1, "{state}");
        assert_eq!(staged[0]["method"], "editor.spawn");
        assert_eq!(staged[0]["id"], reply["staged_id"]);
        let touches = staged[0]["preview"]["touches"].to_string();
        assert!(
            touches.contains("Staged crate"),
            "the preview names the new entity: {touches}"
        );
        // Read-only calls still run immediately.
        let read = h.child_call(id, "editor.history", json!({}));
        assert!(
            read.starts_with("reply:") && !read.contains("\"staged\""),
            "{read}"
        );
        // Calls outside editor.* stay refused in Propose.
        let scene_call = h.child_call(id, "scene.spawn", json!({"name":"Runtime"}));
        assert!(
            scene_call.contains("session_propose_live_unsupported"),
            "{scene_call}"
        );
        assert_eq!(scene(h.app.world_mut()), before);
    }

    /// P-369: approving runs the staged calls in order as one undo entry; one undo removes all.
    #[test]
    fn approving_runs_the_staged_calls_as_one_undo_step() {
        let mut h = Harness::new();
        let id = h.start(Mode::Propose);
        let before = scene(h.app.world_mut());
        let depth = undo_depth(h.app.world());
        for name in ["First staged", "Second staged"] {
            let answer = h.child_call(id, "editor.spawn", json!({"name":name}));
            assert!(answer.contains("\"staged\":true"), "{answer}");
        }
        let reply = h.call(json!({"action":"staged", "session":id, "approve":true}));
        assert_eq!(reply["staged"].as_array().map(Vec::len), Some(0), "{reply}");
        assert_eq!(named(h.app.world_mut(), "First staged"), 1);
        assert_eq!(named(h.app.world_mut(), "Second staged"), 1);
        assert_eq!(
            undo_depth(h.app.world()),
            depth + 1,
            "the approved calls are one undo entry"
        );
        assert!(crate::command::undo_world(h.app.world_mut()), "undo");
        assert_eq!(
            scene(h.app.world_mut()),
            before,
            "one undo reverts every approved call"
        );
        assert!(
            h.app.world().resource::<AgentSessions>().sessions[&id]
                .messages
                .iter()
                .any(|m| m.role == "user" && m.text.contains("Approved 2 staged calls"))
        );
    }

    /// P-369: rejecting discards the staged calls; the scene snapshot is unchanged.
    #[test]
    fn rejecting_leaves_the_world_unchanged() {
        let mut h = Harness::new();
        let entity = h.app.world_mut().spawn_empty().id();
        let id = h.start(Mode::Propose);
        let before = scene(h.app.world_mut());
        let depth = undo_depth(h.app.world());
        let spawn = h.child_call(id, "editor.spawn", json!({"name":"Rejected"}));
        assert!(spawn.contains("\"staged\":true"), "{spawn}");
        let select = h.child_call(id, "editor.select", json!({"entities":[entity.to_bits()]}));
        assert!(select.contains("\"staged\":true"), "{select}");
        let reply = h.call(json!({"action":"staged", "session":id, "approve":false}));
        assert_eq!(reply["staged"].as_array().map(Vec::len), Some(0), "{reply}");
        assert_eq!(scene(h.app.world_mut()), before, "rejecting changed the scene");
        assert_eq!(undo_depth(h.app.world()), depth);
        assert!(h.app.world().resource::<Selection>().is_empty());
        // The child is told; nothing it staged can run later.
        let error = {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (reply, answer) = crossbeam_channel::bounded(1);
            tx.send(regia_agent::server::Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: "editor.agent_sessions".into(),
                    params: json!({"action":"staged", "session":id, "approve":true}),
                },
                reply,
            })
            .expect("request");
            h.app.world_mut().insert_resource(regia_agent::AgentInbox(rx));
            h.app.update();
            answer
                .try_recv()
                .expect("answered")
                .error
                .expect("nothing is staged")
        };
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("session_bad_staged")
        );
        assert_eq!(scene(h.app.world_mut()), before);
    }

    /// Approve a subset by id; the rest stay staged in order until decided.
    #[test]
    fn agent_sessions_approving_one_staged_call_keeps_the_others() {
        let mut h = Harness::new();
        let id = h.start(Mode::Propose);
        let mut ids = Vec::new();
        for name in ["Keep A", "Run B", "Keep C"] {
            let answer = h.child_call(id, "editor.spawn", json!({"name":name}));
            let reply: Value = serde_json::from_str(answer.trim_start_matches("reply:")).expect("JSON reply");
            ids.push(reply["staged_id"].as_u64().expect("staged id"));
        }
        let reply = h.call(json!({"action":"staged", "session":id, "approve":true, "items":[ids[1]]}));
        let left: Vec<u64> = reply["staged"]
            .as_array()
            .expect("staged")
            .iter()
            .filter_map(|s| s["id"].as_u64())
            .collect();
        assert_eq!(left, vec![ids[0], ids[2]]);
        assert_eq!(named(h.app.world_mut(), "Run B"), 1);
        assert_eq!(named(h.app.world_mut(), "Keep A"), 0);
        let reply = h.call(json!({"action":"staged", "session":id, "approve":false, "items":[ids[2]]}));
        assert_eq!(reply["staged"].as_array().map(Vec::len), Some(1));
        h.call(json!({"action":"staged", "session":id, "approve":true}));
        assert_eq!(named(h.app.world_mut(), "Keep A"), 1);
        assert_eq!(named(h.app.world_mut(), "Keep C"), 0);
    }

    /// A failing approved call aborts the group: the calls before it are reverted.
    #[test]
    fn agent_sessions_a_failing_staged_call_reverts_the_group() {
        let mut h = Harness::new();
        let id = h.start(Mode::Propose);
        let before = scene(h.app.world_mut());
        let depth = undo_depth(h.app.world());
        assert!(
            h.child_call(id, "editor.spawn", json!({"name":"Reverted"}))
                .contains("\"staged\":true")
        );
        let bad = h.child_call(id, "editor.spawn", json!({"name":"Bad", "parent": u64::MAX}));
        assert!(bad.contains("\"staged\":true"), "{bad}");
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (reply, answer) = crossbeam_channel::bounded(1);
        tx.send(regia_agent::server::Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "editor.agent_sessions".into(),
                params: json!({"action":"staged", "session":id, "approve":true}),
            },
            reply,
        })
        .expect("request");
        h.app.world_mut().insert_resource(regia_agent::AgentInbox(rx));
        h.app.update();
        let error = answer
            .try_recv()
            .expect("answered")
            .error
            .expect("the group failed");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("session_staged_failed")
        );
        assert_eq!(scene(h.app.world_mut()), before);
        assert_eq!(undo_depth(h.app.world()), depth);
    }

    /// The headless parity arms (agent_parity_shell) expect a staged decision on an unknown session
    /// to be `session_not_found`, and a malformed decision `session_bad_staged`, before any session.
    #[test]
    fn agent_sessions_staged_decisions_validate_before_running() {
        let mut world = World::new();
        crate::agent_ext::install(&mut world);
        let code = |world: &mut World, params: Value| {
            agent_sessions(world, &params)
                .expect_err("refused")
                .data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
        };
        for approve in [true, false] {
            assert_eq!(
                code(
                    &mut world,
                    json!({"action":"staged", "session":0, "approve":approve})
                )
                .as_deref(),
                Some("session_not_found")
            );
            assert_eq!(
                code(
                    &mut world,
                    json!({"action":"staged", "session":0, "approve":approve, "items":[1]})
                )
                .as_deref(),
                Some("session_not_found")
            );
        }
        assert_eq!(
            code(&mut world, json!({"action":"staged", "session":0})).as_deref(),
            Some("session_bad_staged")
        );
        assert_eq!(
            code(
                &mut world,
                json!({"action":"staged", "session":0, "approve":true, "items":"all"})
            )
            .as_deref(),
            Some("session_bad_staged")
        );
    }

    #[test]
    fn agent_sessions_project_change_stops_old_children() {
        let mut h = Harness::new();
        let id = h.start(Mode::Act);
        h.app
            .world_mut()
            .resource_mut::<crate::agent_ext::EditorProject>()
            .root = None;
        tick(h.app.world_mut());
        let sessions = h.app.world().resource::<AgentSessions>();
        assert!(sessions.sessions[&id].process.is_none());
        assert!(!sessions.tokens.is_live(id));
    }
}
