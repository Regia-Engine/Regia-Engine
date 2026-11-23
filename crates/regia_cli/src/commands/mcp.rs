// SPDX-License-Identifier: MPL-2.0
//! `regia mcp [--project DIR] [--attach|--headless] [--http [ADDR]]` — MCP server over stdio (or
//! Streamable HTTP, `mcp_http`) that attaches to a running Regia Engine app, and with `--headless`
//! starts a headless editor when none answers (`docs/specs/23-agent-native.md` § 2, ADR-014,
//! ADR-2346, ADR-2821).
//!
//! A started editor never outlives the adapter. The adapter holds the write end of the editor's
//! stdin and the editor runs with `--exit-on-stdin-eof`, so the kernel's closing of that pipe ends
//! it (saving, as on SIGTERM) however the adapter ends, SIGKILL included, on every platform. On a
//! normal end the adapter also waits for the editor and kills it after `--editor-timeout`.
//!
//! The tool list is generated from the protocol's own schema table (`regia_agent::schema`),
//! so `agent.hello`, `agent.schema` and MCP can never drift apart. MCP resources (§ 3.8)
//! forward to the matching protocol methods.

use std::future::Future;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use regia_agent::client::{Client, ClientError};
use regia_agent::schema;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListResourcesResult, ListToolsResult, ReadResourceRequestParams, ReadResourceResponse, Resource,
        ResourceContents, ResourceTemplate, ServerCapabilities, ServerConfig, Tool,
    },
    service::{MaybeSendFuture, RequestContext},
};
use serde_json::{Value, json};
use tokio::task;

/// The default of `--editor-timeout`: a cold editor on a loaded machine opens a device and its asset
/// database first.
pub const DEFAULT_EDITOR_TIMEOUT_SECS: u64 = 120;

/// Arguments of `regia mcp`.
#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct McpArgs {
    /// A helper instead of a server (`regia mcp token`).
    #[command(subcommand)]
    pub command: Option<McpCommand>,
    /// Project directory (default current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Attach to a running app and never start one (also what no flag does).
    #[arg(long, conflicts_with = "headless")]
    pub attach: bool,
    /// When no app of this project answers, start a headless editor for it, wait for its ready
    /// line and attach to it; it exits with this adapter. A running app is attached to and left
    /// alone.
    #[arg(long)]
    pub headless: bool,
    /// With --headless: the started editor opens no GPU device (view methods answer
    /// `editor_headless`).
    #[arg(long, requires = "headless")]
    pub no_gpu: bool,
    /// With --headless: the editor binary (default: `REGIA_EDITOR`, then `regia-editor` next to
    /// this `regia`, then on `PATH`).
    #[arg(long, value_name = "FILE", requires = "headless")]
    pub editor_bin: Option<PathBuf>,
    /// With --headless: seconds to wait for the editor's ready line, and for it to exit at the end.
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_EDITOR_TIMEOUT_SECS, requires = "headless")]
    pub editor_timeout: u64,
    /// Serve MCP over Streamable HTTP instead of stdio, on ADDR (default 127.0.0.1 on a free port;
    /// the URL and token file are written to .regia/agent/mcp_http.json).
    #[arg(long, value_name = "ADDR", num_args = 0..=1, default_missing_value = "127.0.0.1:0")]
    pub http: Option<std::net::SocketAddr>,
    /// With --http: allow a non-loopback ADDR (the bearer token then guards the endpoint).
    #[arg(long, requires = "http")]
    pub allow_remote: bool,
    /// With --http: also accept requests whose Origin header is this (repeatable); loopback
    /// origins are always accepted.
    #[arg(long = "allow-origin", value_name = "ORIGIN", requires = "http")]
    pub allow_origins: Vec<String>,
    /// With --http: the largest request body in bytes.
    #[arg(long, value_name = "BYTES", default_value_t = regia_agent::mcp_http::DEFAULT_MAX_BODY_BYTES, requires = "http")]
    pub http_max_body: usize,
    /// With --http: read the bearer token from this file instead of minting one per launch.
    #[arg(long, value_name = "FILE", requires = "http")]
    pub token_file: Option<PathBuf>,
}

/// The helpers of `regia mcp`.
#[derive(Debug, Clone, Subcommand)]
pub enum McpCommand {
    /// Print the bearer token (or with --url the URL) of the running `regia mcp --http`, after
    /// checking its advertisement: a loopback URL and the token file minted in .regia/agent/.
    Token(McpTokenArgs),
}

/// Arguments of `regia mcp token`.
#[derive(Debug, Clone, Args)]
pub struct McpTokenArgs {
    /// Project directory (default current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print the endpoint URL instead of the token.
    #[arg(long)]
    pub url: bool,
}

/// Runs `regia mcp` as an async fn, returning after the MCP server loop ends.
pub async fn run(args: &McpArgs) -> Result<()> {
    if let Some(McpCommand::Token(token)) = &args.command {
        let advert = super::mcp_http::read_advert(&token.project)?;
        println!("{}", if token.url { &advert.url } else { &advert.token });
        return Ok(());
    }
    let project_dir = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    // Held for the whole run: dropping it, or this process ending any other way, ends the editor.
    let editor = if args.headless && !args.attach {
        let args = args.clone();
        let project = project_dir.clone();
        task::spawn_blocking(move || ManagedEditor::ensure(&args, &project))
            .await
            .map_err(|e| anyhow::anyhow!("task join error: {e}"))??
    } else {
        None
    };
    let result = serve(args, project_dir).await;
    if let Some(editor) = editor {
        let _ = task::spawn_blocking(move || drop(editor)).await;
    }
    result
}

/// Serves MCP on the chosen transport until the client goes (stdio) or the process ends (HTTP).
async fn serve(args: &McpArgs, project_dir: PathBuf) -> Result<()> {
    if let Some(bind) = args.http {
        let options = super::mcp_http::HttpOptions {
            bind,
            allow_remote: args.allow_remote,
            allowed_origins: args.allow_origins.clone(),
            max_body: args.http_max_body,
            token_file: args.token_file.clone(),
        };
        return super::mcp_http::serve(project_dir, &options).await;
    }
    let server = McpServer::new(project_dir);
    let (stdin, stdout) = rmcp::transport::io::stdio();
    let running = rmcp::serve_server(server, (stdin, stdout))
        .await
        .map_err(|e| anyhow::anyhow!("MCP server failed to start: {e}"))?;
    running
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server loop ended: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// A headless editor this adapter started (ADR-2821)
// ---------------------------------------------------------------------------

/// Writes one line to stderr. Never panics: the client that reads it may be gone.
fn note(text: &str) {
    let _ = writeln!(std::io::stderr(), "regia mcp: {text}");
}

/// A running editor this adapter started. Dropping it ends the editor.
struct ManagedEditor {
    child: Child,
    /// The write end of the editor's stdin: closing it asks the editor to save and exit.
    stdin: Option<ChildStdin>,
    /// How long the end of the run waits for the editor.
    exit_timeout: Duration,
}

impl ManagedEditor {
    /// Attaches when an app of `project` answers (`Ok(None)`), else starts a headless editor.
    fn ensure(args: &McpArgs, project: &Path) -> Result<Option<Self>> {
        if Client::connect_project(project).is_ok() {
            note("a running app answers; attaching to it");
            return Ok(None);
        }
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let program = super::editor::resolve_editor_bin(
            args.editor_bin.as_deref(),
            regia_core::brand::env_var("REGIA_EDITOR").ok().map(PathBuf::from),
            sibling.as_deref(),
        );
        let editor_args = super::editor::EditorArgs {
            command: None,
            project_dir: None,
            project: None,
            headless: true,
            no_gpu: args.no_gpu,
            game: None,
            exit_after_frames: None,
            editor_bin: None,
        };
        let mut argv = super::editor::editor_argv(&editor_args, project);
        argv.push("--exit-on-stdin-eof".to_string());
        Self::start(&program, &argv, Duration::from_secs(args.editor_timeout)).map(Some)
    }

    /// Starts `program argv` and waits for its ready line.
    fn start(program: &Path, argv: &[String], timeout: Duration) -> Result<Self> {
        let mut child = Command::new(program)
            .args(argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!(
                    "mcp_editor_start_failed: cannot start {}: {e} (build it with `cargo build -p regia_editor`, or pass --editor-bin / set REGIA_EDITOR)",
                    program.display()
                )
            })?;
        let stdin = child.stdin.take();
        let mut editor = Self {
            child,
            stdin,
            exit_timeout: timeout,
        };
        // The reader outlives the wait: a full pipe must never block the editor.
        let (lines, ready_lines) = mpsc::channel();
        if let Some(stdout) = editor.child.stdout.take() {
            std::thread::Builder::new()
                .name("regia-mcp-editor-out".into())
                .spawn(move || read_editor_output(stdout, &lines))
                .context("cannot start the editor output reader")?;
        }
        let ready = wait_for_ready(&ready_lines, &mut editor.child, timeout);
        match ready {
            Ok(ready) => {
                note(&format!(
                    "started a headless editor (pid {}): {}",
                    editor.child.id(),
                    ready["url"].as_str().unwrap_or("no url")
                ));
                Ok(editor)
            }
            Err(error) => {
                editor.kill();
                Err(error)
            }
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ManagedEditor {
    fn drop(&mut self) {
        // End of stream: the editor saves its dirty documents and exits.
        self.stdin = None;
        match wait_for_exit(&mut self.child, self.exit_timeout) {
            Some(status) => note(&format!("the headless editor exited: {status}")),
            None => {
                note("the headless editor did not exit in time; killing it");
                self.kill();
            }
        }
    }
}

/// Waits for `child` to exit, up to `timeout`.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() >= deadline => return None,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
}

/// Hands the editor's stdout lines to `lines` and to stderr, until the editor closes it.
fn read_editor_output(stdout: std::process::ChildStdout, lines: &mpsc::Sender<String>) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end();
                note(&format!("editor: {line}"));
                // Nobody waits for lines once the editor is ready.
                let _ = lines.send(line.to_string());
            }
        }
    }
}

/// Waits for the `{"event":"ready",...}` line.
///
/// # Errors
/// `mcp_editor_exited` when the editor ends first, `mcp_editor_ready_timeout` after `timeout`.
fn wait_for_ready(lines: &mpsc::Receiver<String>, child: &mut Child, timeout: Duration) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    let mut open = true;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            anyhow::bail!(
                "mcp_editor_ready_timeout: the editor printed no ready line in {} s (raise it with --editor-timeout)",
                timeout.as_secs()
            );
        }
        if open {
            match lines.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(line) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&line)
                        && value["event"] == "ready"
                    {
                        return Ok(value);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => open = false,
            }
        } else {
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Lines already read come first: a ready line just before an exit still counts for
            // nothing, as there is no editor left to attach to.
            anyhow::bail!("mcp_editor_exited: the editor ended before its ready line: {status}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tools: generated from the protocol schema table (specs/23 § 3)
// ---------------------------------------------------------------------------

/// The MCP tool list, one per protocol method that has a tool name.
fn tools() -> Vec<Tool> {
    schema::METHODS
        .iter()
        .filter(|method| !method.tool.is_empty())
        .map(|method| {
            let schema = method.params_json_schema();
            let Some(map) = schema.as_object() else {
                // params_json_schema always builds an object schema.
                unreachable!("schema params are always an object");
            };
            Tool::new(method.tool, method.description, map.clone())
        })
        .collect()
}

/// The attached app's own methods (the editor's `editor.*`, ADR-5628) as MCP tools, read from
/// its live `agent.schema` table: every entry that is not a built-in method and carries a tool
/// name no built-in method uses (a built-in name wins, as in `agent.schema`). Answers each tool
/// with the JSON-RPC method it calls, in table order, each tool name once.
fn host_tools(schema_table: &Value) -> Vec<(Tool, String)> {
    let mut seen = std::collections::BTreeSet::new();
    let Some(entries) = schema_table.get("methods").and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?;
            let tool = entry.get("tool")?.as_str()?;
            if tool.is_empty()
                || schema::method(name).is_some()
                || schema::method_by_tool(tool).is_some()
                || !seen.insert(tool.to_string())
            {
                return None;
            }
            let description = entry.get("description").and_then(Value::as_str).unwrap_or(name);
            let params = match entry.get("params") {
                Some(Value::Object(map)) if map.contains_key("type") => map.clone(),
                _ => {
                    let Value::Object(map) = json!({ "type": "object" }) else {
                        return None;
                    };
                    map
                }
            };
            Some((
                Tool::new(tool.to_string(), description.to_string(), params),
                name.to_string(),
            ))
        })
        .collect()
}

/// Asks the attached app for its `agent.schema` table and returns its host tools; no app, or an
/// app that refuses the call, has none (the built-in tools still work without an app).
fn fetch_host_tools(conn: &Mutex<AppConnection>) -> Option<Vec<(Tool, String)>> {
    let mut guard = conn.lock().unwrap_or_else(PoisonError::into_inner);
    match guard.call("agent.schema", &json!({})) {
        Ok(table) => Some(host_tools(&table)),
        Err(failure) => {
            tracing::debug!(target: "regia_mcp", "no host tools: {}", failure.message());
            None
        }
    }
}

/// Tools whose result carries a PNG under this key answer with an image content block.
fn png_key_of_tool(tool: &str) -> Option<&'static str> {
    match tool {
        "screenshot" | "capture_sequence" | "contact_sheet" => Some("png_base64"),
        "compare_images" => Some("diff_png_base64"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Resources (specs/23 § 3.8)
// ---------------------------------------------------------------------------

/// A static resource, with the protocol method that serves it.
struct StaticResource {
    /// MCP resource URI, e.g. `regia://project`.
    uri: &'static str,
    /// Short resource name.
    name: &'static str,
    /// One-line description.
    description: &'static str,
    /// MIME type of the served text.
    mime_type: &'static str,
    /// The protocol method to call, and params for it.
    method: &'static str,
    params: Value,
    /// Which result field to serve as the text (the whole result when `None`).
    field: Option<&'static str>,
}

/// The fixed resources of `regia://` (specs/23 § 3.8).
const RESOURCES: &[StaticResource] = &[
    StaticResource {
        uri: "regia://project",
        name: "project",
        description: "Engine, app and project identity, subsystems and the agent configuration.",
        mime_type: "application/json",
        method: "project.info",
        params: Value::Null,
        field: None,
    },
    StaticResource {
        uri: "regia://scene/current",
        name: "scene/current",
        description: "The current scene as canonical RON.",
        mime_type: "text/plain",
        method: "scene.serialize",
        params: Value::Null,
        field: Some("ron"),
    },
    StaticResource {
        uri: "regia://logs/recent",
        name: "logs/recent",
        description: "Recent engine log records.",
        mime_type: "application/json",
        method: "logs.read",
        params: Value::Null,
        field: None,
    },
    StaticResource {
        uri: "regia://protocol/schema.json",
        name: "protocol/schema",
        description: "The full protocol schema: every method, its parameters and permissions.",
        mime_type: "application/schema+json",
        method: "agent.schema",
        params: Value::Null,
        field: None,
    },
];

/// Resource templates (parameterised URIs), with the method and the param its argument fills.
struct TemplatedResource {
    /// RFC 6570 URI template.
    template: &'static str,
    /// The path segment pattern this template matches (`regia://types/{path}` → "types").
    prefix: &'static str,
    /// Short template name.
    name: &'static str,
    /// Description.
    description: &'static str,
    /// The protocol method to call.
    method: &'static str,
    /// Which parameter of that method the path suffix fills.
    param: &'static str,
}

/// The templated resources of `regia://` (specs/23 § 3.8).
const TEMPLATES: &[TemplatedResource] = &[
    TemplatedResource {
        template: "regia://types/{path}",
        prefix: "types",
        name: "types",
        description: "JSON Schema, docs and an example of one reflected type.",
        method: "types.describe",
        param: "type",
    },
    TemplatedResource {
        template: "regia://assets/{id}",
        prefix: "assets",
        name: "assets",
        description: "Manifest entry and load state of one asset (path or id).",
        method: "assets.describe",
        param: "asset",
    },
    TemplatedResource {
        template: "regia://guide/{topic}",
        prefix: "guide",
        name: "guide",
        description: "One bundled agent-guide topic.",
        method: "agent.guide",
        param: "topic",
    },
];

// ---------------------------------------------------------------------------
// Shared state: lazy connection to the running app
// ---------------------------------------------------------------------------

/// Holds the lazily connected client and the project directory.
struct AppConnection {
    project_dir: PathBuf,
    /// `None` until a tool call connects, and again after a connection failure.
    client: Option<Client>,
}

/// Why a tool call failed. Every variant becomes a tool error the calling model can read.
enum CallFailure {
    /// No app advertised an endpoint, or connecting failed.
    NoApp(String),
    /// The app answered with a protocol error (bad parameters, unknown entity, …).
    Rpc(String),
    /// The connection broke and reconnecting did not help.
    Connection(String),
}

impl CallFailure {
    fn message(&self) -> String {
        match self {
            Self::NoApp(e) => format!(
                "no running Regia Engine app found ({e}). Start `regia mcp --headless` (it starts a \
                 headless editor), or start an app such as `regia editor --headless` or `cargo run -p \
                 hello_window`, then retry."
            ),
            Self::Rpc(e) => e.clone(),
            Self::Connection(e) => format!("connection to the app lost ({e}); is it still running?"),
        }
    }
}

impl AppConnection {
    fn new(project_dir: PathBuf) -> Self {
        Self {
            project_dir,
            client: None,
        }
    }

    /// Calls `method`. A broken connection is re-established once per call (the app may have been
    /// restarted); protocol errors are returned as they are, without retrying.
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, CallFailure> {
        for attempt in 0..2 {
            if self.client.is_none() {
                match Client::connect_project(&self.project_dir) {
                    Ok(c) => self.client = Some(c),
                    Err(e) => return Err(CallFailure::NoApp(e.to_string())),
                }
            }
            let Some(client) = self.client.as_mut() else {
                continue;
            };
            match client.call(method, params.clone()) {
                Ok(v) => return Ok(v),
                Err(ClientError::Rpc(e)) => {
                    let hint = e
                        .data
                        .as_ref()
                        .and_then(|d| d.get("hint"))
                        .and_then(Value::as_str);
                    let code = e
                        .data
                        .as_ref()
                        .and_then(|d| d.get("code"))
                        .and_then(Value::as_str);
                    return Err(CallFailure::Rpc(match (code, hint) {
                        (Some(code), Some(hint)) => format!("{} [{code}] — {hint}", e.message),
                        _ => e.message,
                    }));
                }
                Err(e) => {
                    self.client = None;
                    if attempt == 1 {
                        return Err(CallFailure::Connection(e.to_string()));
                    }
                    tracing::info!(target: "regia_mcp", "connection failed ({e}); reconnecting once");
                }
            }
        }
        Err(CallFailure::Connection("reconnect failed".into()))
    }
}

// ---------------------------------------------------------------------------
// The MCP server struct
// ---------------------------------------------------------------------------

/// The `regia mcp` server implementing `rmcp::ServerHandler`.
pub struct McpServer {
    /// Tool definitions returned by `list_tools`.
    tools: Vec<Tool>,
    /// Shared mutable connection state.
    conn: Arc<Mutex<AppConnection>>,
    /// The attached app's host tools (tool name → JSON-RPC method) from its last `agent.schema`.
    host: Arc<Mutex<std::collections::BTreeMap<String, String>>>,
}

impl McpServer {
    /// Build the server for `project_dir`.
    pub fn new(project_dir: PathBuf) -> Self {
        Self {
            tools: tools(),
            conn: Arc::new(Mutex::new(AppConnection::new(project_dir))),
            host: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
        }
    }
}

/// Re-reads the attached app's host tools into `host` and returns them; `None` (and `host`
/// unchanged) when no app answers.
fn refresh_host_tools(
    conn: &Mutex<AppConnection>,
    host: &Mutex<std::collections::BTreeMap<String, String>>,
) -> Option<Vec<Tool>> {
    let found = fetch_host_tools(conn)?;
    let mut map = host.lock().unwrap_or_else(PoisonError::into_inner);
    map.clear();
    let mut tools = Vec::with_capacity(found.len());
    for (tool, method) in found {
        map.insert(tool.name.to_string(), method);
        tools.push(tool);
    }
    Some(tools)
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(
            "Regia engine agent protocol bridge (specs/23). Attaches to a running Regia Engine app \
             (`regia mcp --headless` starts a headless editor when none answers). Call agent.hello (tool: hello) to \
             see protocol version and capabilities; agent.schema/guide describe every method. \
             Long operations answer a job id (job_status polls it); review_propose stages file \
             changes for the owner's review.",
        )
        .with_server_info(Implementation::new("regia-mcp", env!("CARGO_PKG_VERSION")))
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + MaybeSendFuture + '_ {
        let mut tools = self.tools.clone();
        let conn = self.conn.clone();
        let host = self.host.clone();
        async move {
            // The attached app's own methods (the editor's) follow the built-in tools (ADR-5628).
            if let Ok(Some(extra)) = task::spawn_blocking(move || refresh_host_tools(&conn, &host)).await {
                tools.extend(extra);
            }
            Ok(ListToolsResult::with_all_items(tools))
        }
    }

    fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> + MaybeSendFuture + '_ {
        let resources: Vec<Resource> = RESOURCES
            .iter()
            .map(|r| {
                Resource::new(r.uri, r.name)
                    .with_description(r.description)
                    .with_mime_type(r.mime_type)
            })
            .collect();
        async move { Ok(ListResourcesResult::with_all_items(resources)) }
    }

    fn list_resource_templates(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::ListResourceTemplatesResult, McpError>> + MaybeSendFuture + '_
    {
        let templates: Vec<ResourceTemplate> = TEMPLATES
            .iter()
            .map(|t| ResourceTemplate::new(t.template, t.name).with_description(t.description))
            .collect();
        async move {
            Ok(rmcp::model::ListResourceTemplatesResult::with_all_items(
                templates,
            ))
        }
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, McpError>> + MaybeSendFuture + '_ {
        let conn = self.conn.clone();
        let uri = request.uri.clone();
        async move {
            let Some((method, params, field)) = resource_call_for(&uri) else {
                return Err(McpError::resource_not_found(
                    format!("no regia:// resource {uri} (see resources/list)"),
                    None,
                ));
            };
            let call_method = method.clone();
            let joined = task::spawn_blocking(move || {
                let mut guard = conn.lock().unwrap_or_else(PoisonError::into_inner);
                guard.call(&call_method, &params)
            })
            .await;
            let value = match joined {
                Ok(Ok(value)) => value,
                Ok(Err(failure)) => {
                    return Err(McpError::internal_error(failure.message(), None));
                }
                Err(join_err) => {
                    return Err(McpError::internal_error(
                        format!("task join error: {join_err}"),
                        None,
                    ));
                }
            };
            let text = match field {
                Some(field) => {
                    let Some(text) = value.get(field).and_then(Value::as_str) else {
                        return Err(McpError::internal_error(
                            format!("the app's {method} result has no {field} text"),
                            None,
                        ));
                    };
                    text.to_string()
                }
                None => serde_json::to_string_pretty(&value).unwrap_or_else(|_| format!("{value:?}")),
            };
            Ok(ReadResourceResponse::Complete(
                rmcp::model::ReadResourceResult::new(vec![ResourceContents::text(text, uri)]),
            ))
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, McpError>> + MaybeSendFuture + '_ {
        let conn = self.conn.clone();
        let host = self.host.clone();
        let tool_name = request.name.to_string();
        let arguments = request.arguments.clone();
        async move {
            let params: Value = arguments.map(Value::Object).unwrap_or(Value::Null);
            let lookup = tool_name.clone();
            // The protocol client is blocking: run it off the async runtime.
            let joined = task::spawn_blocking(move || {
                // A built-in tool, else one of the attached app's host tools (read again once
                // when the name is not known yet: a client may call without listing first).
                let method = match schema::method_by_tool(&lookup) {
                    Some(m) => m.name.to_string(),
                    None => {
                        let known = host
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .get(&lookup)
                            .cloned();
                        known.or_else(|| {
                            refresh_host_tools(&conn, &host)?;
                            host.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .get(&lookup)
                                .cloned()
                        })?
                    }
                };
                let mut guard = conn.lock().unwrap_or_else(PoisonError::into_inner);
                Some(guard.call(&method, &params))
            })
            .await;
            let joined = match joined {
                Ok(None) => {
                    return Err(McpError::method_not_found::<rmcp::model::CallToolRequestMethod>());
                }
                Ok(Some(result)) => Ok(result),
                Err(join_err) => Err(join_err),
            };
            let call_result = match joined {
                Ok(Ok(value)) => value,
                Ok(Err(failure)) => {
                    return Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                        ContentBlock::text(failure.message()),
                    ])));
                }
                Err(join_err) => {
                    return Err(McpError::internal_error(
                        format!("task join error: {join_err}"),
                        None,
                    ));
                }
            };

            // Image tools answer with an image content block plus a short caption.
            if let Some(key) = png_key_of_tool(&tool_name)
                && let Some(b64) = call_result.get(key).and_then(Value::as_str)
            {
                let width = call_result.get("width").and_then(Value::as_u64).unwrap_or(0);
                let height = call_result.get("height").and_then(Value::as_u64).unwrap_or(0);
                let mut blocks = vec![ContentBlock::image(b64, "image/png")];
                let caption = match tool_name.as_str() {
                    "screenshot" => format!("Screenshot: {width}x{height}"),
                    "capture_sequence" => format!(
                        "Sequence strip: {} frames, {width}x{height}",
                        call_result.get("frames").and_then(Value::as_u64).unwrap_or(0)
                    ),
                    "contact_sheet" => format!(
                        "Contact sheet: {} views, {width}x{height}",
                        call_result.get("angles").and_then(Value::as_u64).unwrap_or(0)
                    ),
                    "compare_images" => format!(
                        "SSIM {}, max channel diff {} (difference image; {width}x{height})",
                        call_result.get("ssim").and_then(Value::as_f64).unwrap_or(1.0),
                        call_result
                            .get("max_channel_diff")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    ),
                    other => other.to_string(),
                };
                blocks.push(ContentBlock::text(caption));
                return Ok(CallToolResponse::Complete(CallToolResult::success(blocks)));
            }

            let text =
                serde_json::to_string_pretty(&call_result).unwrap_or_else(|_| format!("{call_result:?}"));
            Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text(text),
            ])))
        }
    }
}

/// The resource URI scheme from before the rename (ADR-1388, ADR-1421): read for one release,
/// so an agent with an old URI written down still gets the resource.
const LEGACY_URI_SCHEME: &str = "ferrite://"; // rename-keep

/// Maps a `regia://` resource URI (or one with the legacy scheme) to the protocol call that
/// serves it: `(method, params, result-field-to-serve)`.
fn resource_call_for(uri: &str) -> Option<(String, Value, Option<&'static str>)> {
    let current;
    let uri = match uri.strip_prefix(LEGACY_URI_SCHEME) {
        Some(rest) => {
            current = format!("regia://{rest}");
            current.as_str()
        }
        None => uri,
    };
    for resource in RESOURCES {
        if resource.uri == uri {
            return Some((
                resource.method.to_string(),
                resource.params.clone(),
                resource.field,
            ));
        }
    }
    let rest = uri.strip_prefix("regia://")?;
    let (prefix, argument) = rest.split_once('/')?;
    if argument.is_empty() {
        return None;
    }
    for template in TEMPLATES {
        if template.prefix == prefix {
            let mut params = json!({});
            params[template.param] = json!(argument);
            return Some((template.method.to_string(), params, None));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_agent::protocol::{Endpoint, Response, RpcError};
    use regia_agent::schema::MethodSchema;

    /// A protocol error reaches the model as a readable message, without a reconnect or retry.
    #[test]
    fn protocol_errors_are_returned_not_retried() {
        let token = "t0ken".to_string();
        let server = regia_agent::server::start(token.clone()).expect("server");
        let dir = std::env::temp_dir().join(format!("regia-mcp-rpc-{}", std::process::id()));
        let agent_dir = regia_agent::endpoint::agent_dir(&dir);
        let endpoint = Endpoint {
            url: format!("ws://{}", server.addr),
            pid: 1,
            protocol: "0.2".into(),
            token_file: agent_dir.join("token").display().to_string(),
            app: "test".into(),
        };
        regia_agent::endpoint::write(&agent_dir, &endpoint, &token).expect("endpoint");
        let inbox = server.inbox.clone();
        let answered = std::thread::spawn(move || {
            let mut n = 0;
            while let Ok(inbound) = inbox.recv_timeout(std::time::Duration::from_secs(2)) {
                n += 1;
                let id = inbound.request.id.clone().unwrap_or(Value::Null);
                let _ = inbound.reply.send(Response::err(
                    id,
                    RpcError::not_found("no field `x`", "check the names"),
                ));
            }
            n
        });
        let mut conn = AppConnection::new(dir.clone());
        let failure = conn.call("scene.set_property", &json!({ "path": "x" })).err();
        let message = failure.map(|f| f.message()).unwrap_or_default();
        assert!(
            message.contains("no field `x`") && message.contains("F0003"),
            "{message}"
        );
        drop(conn);
        assert_eq!(answered.join().unwrap_or(0), 1, "exactly one request, no retry");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tool_list_is_the_schema_table() {
        let tools = tools();
        let expected: Vec<&str> = schema::METHODS
            .iter()
            .filter(|m: &&MethodSchema| !m.tool.is_empty())
            .map(|m| m.tool)
            .collect();
        assert_eq!(tools.len(), expected.len(), "one tool per schema method");
        for tool in &tools {
            let method = schema::method_by_tool(tool.name.as_ref()).expect("tool maps to a method");
            assert_eq!(method.tool, tool.name.as_ref());
            assert!(
                tool.description.as_ref().is_some_and(|d| !d.is_empty()),
                "{} described",
                tool.name
            );
            assert!(tool.input_schema.contains_key("type"), "{} typed", tool.name);
        }
        for tool in [
            "hello",
            "spawn",
            "wait_until",
            "expect",
            "capture_sequence",
            "read_events",
        ] {
            assert!(
                expected.contains(&tool),
                "the M11 surface is exposed ({tool} missing)"
            );
        }
    }

    /// Lines before the ready line are skipped, whatever they are.
    #[test]
    fn the_ready_line_is_found_among_other_output() {
        let (tx, rx) = mpsc::channel();
        for line in [
            "not json",
            "{\"event\":\"other\"}",
            "{\"event\":\"ready\",\"url\":\"ws://x\"}",
        ] {
            tx.send(line.to_string()).expect("line");
        }
        // A real child to poll: this test's own binary listing its tests ends at once, so a wait
        // that looks at it first could only fail by reading the lines too late.
        let mut child = Command::new(std::env::current_exe().expect("exe"))
            .arg("--list")
            .stdout(Stdio::null())
            .spawn()
            .expect("child");
        let ready = wait_for_ready(&rx, &mut child, Duration::from_secs(600)).expect("ready");
        assert_eq!(ready["url"], "ws://x");
        let _ = child.wait();
    }

    #[test]
    fn unknown_tools_are_rejected_by_lookup() {
        assert!(schema::method_by_tool("unknown_tool").is_none());
    }

    /// The attached editor's methods become MCP tools (spec 23 § 2/§ 3, ADR-5628): built-in
    /// method names and built-in tool names win, tool-less and repeated entries are skipped, and
    /// each tool keeps its method, description and params schema.
    #[test]
    fn host_tools_add_the_editors_methods_and_built_ins_win() {
        let table = json!({
            "methods": [
                { "name": "agent.hello", "tool": "hello", "description": "built in" },
                { "name": "editor.save_level", "tool": "save_level", "description": "Save the level.",
                  "params": { "type": "object", "properties": { "path": { "type": "string" } },
                              "required": [], "additionalProperties": false } },
                // The editor's PIE pause uses the runtime's tool name: the runtime method keeps it.
                { "name": "editor.pause", "tool": "pause", "description": "Pause PIE." },
                { "name": "editor.play_state", "tool": "", "description": "no tool" },
                { "name": "editor.undo", "tool": "undo" },
                { "name": "editor.undo_again", "tool": "undo", "description": "repeated tool" },
                { "name": "editor.no_tool_field" },
            ],
            "extension_methods": [ { "name": "ext.verb", "description": "no tool field" } ],
        });
        let found = host_tools(&table);
        let pairs: Vec<(&str, &str)> = found
            .iter()
            .map(|(tool, method)| (tool.name.as_ref(), method.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![("save_level", "editor.save_level"), ("undo", "editor.undo")]
        );
        let (save, _) = &found[0];
        assert_eq!(save.description.as_deref(), Some("Save the level."));
        assert!(save.input_schema["properties"]["path"].is_object());
        // An entry without a description or params schema still gets a usable tool.
        let (undo, _) = &found[1];
        assert_eq!(undo.description.as_deref(), Some("editor.undo"));
        assert_eq!(undo.input_schema["type"], "object");
        // Every built-in tool still maps to its runtime method.
        assert_eq!(
            schema::method_by_tool("pause").map(|m| m.name),
            Some("play.pause")
        );
    }

    #[test]
    fn host_tools_of_a_table_without_methods_are_empty() {
        assert!(host_tools(&json!({})).is_empty());
        assert!(host_tools(&json!({ "methods": "not an array" })).is_empty());
        assert!(host_tools(&Value::Null).is_empty());
    }

    #[test]
    fn image_tools_carry_a_png_key() {
        assert_eq!(png_key_of_tool("screenshot"), Some("png_base64"));
        assert_eq!(png_key_of_tool("compare_images"), Some("diff_png_base64"));
        assert_eq!(png_key_of_tool("hello"), None);
        assert_eq!(png_key_of_tool("telemetry_screenshot_mark"), None);
    }

    #[test]
    fn resource_uris_map_to_protocol_calls() {
        let (method, _params, field) = resource_call_for("regia://scene/current").expect("scene resource");
        assert_eq!(method, "scene.serialize");
        assert_eq!(field, Some("ron"));

        let (method, params, field) =
            resource_call_for("regia://types/regia_scene::Transform").expect("type resource");
        assert_eq!(method, "types.describe");
        assert_eq!(params["type"], "regia_scene::Transform");
        assert_eq!(field, None);

        let (method, params, _) =
            resource_call_for("regia://assets/assets/khronos/helmet.glb").expect("asset");
        assert_eq!(method, "assets.describe");
        assert_eq!(params["asset"], "assets/khronos/helmet.glb");

        let (method, params, _) = resource_call_for("regia://guide/testing").expect("guide");
        assert_eq!(method, "agent.guide");
        assert_eq!(params["topic"], "testing");

        assert!(resource_call_for("regia://nope").is_none());
        assert!(resource_call_for("http://example.com").is_none());
        assert!(resource_call_for("regia://types/").is_none(), "empty argument");
    }

    /// URIs with the scheme from before the rename still map, to the same calls (ADR-1421).
    #[test]
    fn legacy_scheme_uris_map_like_current_ones() {
        let legacy = |path: &str| format!("{LEGACY_URI_SCHEME}{path}");
        assert_eq!(
            resource_call_for(&legacy("scene/current")),
            resource_call_for("regia://scene/current")
        );
        let (method, params, _) = resource_call_for(&legacy("guide/testing")).expect("legacy guide");
        assert_eq!(method, "agent.guide");
        assert_eq!(params["topic"], "testing");
        for resource in RESOURCES {
            let path = resource.uri.strip_prefix("regia://").expect("current scheme");
            assert!(
                resource_call_for(&legacy(path)).is_some(),
                "{path} maps with the legacy scheme"
            );
        }
        assert!(resource_call_for(&legacy("nope")).is_none());
    }

    /// ADR-1917: the JSON-RPC frame is fuzzed by the `agent.request` target and rmcp parses the MCP frames; a resource
    /// URI is the one string this adapter parses itself. A mutated URI must end in a real method or in nothing.
    #[test]
    fn mutated_resource_uris_reach_real_methods_or_nothing() {
        let mut rng = regia_core::fuzz::FuzzRng::new(0x0917_0003);
        let listed = RESOURCES.iter().map(|r| r.uri.to_string());
        let templated = TEMPLATES.iter().map(|t| format!("regia://{}/argument", t.prefix));
        let mut mapped = 0;
        for uri in listed.chain(templated) {
            for _ in 0..300 {
                let mutated = regia_core::fuzz::mutate_text(&uri, &mut rng);
                if let Some((method, _, _)) = resource_call_for(&mutated) {
                    mapped += 1;
                    assert!(
                        schema::method(&method).is_some(),
                        "{mutated:?} maps to {method}, which is no method"
                    );
                }
            }
        }
        assert!(mapped > 0, "no mutation mapped: the check never ran");
    }

    #[test]
    fn every_listed_resource_maps_to_a_call() {
        for resource in RESOURCES {
            assert!(resource_call_for(resource.uri).is_some(), "{} maps", resource.uri);
        }
        for template in TEMPLATES {
            let example = format!("regia://{}/example", template.prefix);
            assert!(resource_call_for(&example).is_some(), "{example} maps");
        }
    }
}
