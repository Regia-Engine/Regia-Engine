// SPDX-License-Identifier: MPL-2.0
//! `regia mcp --http`: the MCP Streamable HTTP transport next to stdio (`docs/specs/23-agent-native.md`
//! § 2, ADR-2346).
//!
//! One endpoint (`/mcp`) takes one JSON-RPC message per `POST` and answers a request with its
//! response as `application/json`, a notification or response with `202 Accepted`. `initialize`
//! opens a session (`Mcp-Session-Id`); every later message names it; `DELETE` ends it. There is no
//! server-to-client event stream (`GET` answers `405`, which the transport allows).
//!
//! Each session runs the same [`super::mcp::McpServer`] as stdio over an in-memory pipe, so the tool
//! list, resources and errors are identical by construction. The request judging (head parser,
//! Origin and Host checks, bearer token, size and type limits) is `regia_agent::mcp_http`, fuzzed
//! there. The server binds `127.0.0.1` on a free port unless told otherwise, refuses a non-loopback
//! bind without `--allow-remote`, mints a token per launch into `.regia/agent/mcp_http_token`
//! (mode 0600) unless `--token-file` names one, and advertises itself in `.regia/agent/mcp_http.json`.
//!
//! A connection is *pre-auth* until its first request passes the token check: pre-auth connections
//! have a pool of their own ([`MAX_PREAUTH_CONNECTIONS`]; when it is full the oldest is closed for
//! the newcomer), a shorter deadline for that first head, and a cap on the reads the head may take
//! ([`MAX_HEAD_READS`]). Only an admitted connection takes one of the [`MAX_CONNECTIONS`] slots, so
//! idle or trickling sockets from anyone who can reach the port cannot keep the token holder out
//! (threat model finding 24, ADR-3285).
//!
//! `regia mcp token` ([`read_advert`]) prints the token or URL of the advertisement after checking
//! it the way `regia_agent::endpoint::read` checks the WebSocket one: a loopback URL and the minted
//! token file inside `.regia/agent/` (finding 31).

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use regia_agent::mcp_http::{self, Admitted, BodyKind, HttpPolicy, HttpRefusal, RequestHead};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, WriteHalf,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

/// Most concurrent sessions.
pub const MAX_SESSIONS: usize = 16;

/// Most concurrent admitted connections (a request on them passed the token check).
pub const MAX_CONNECTIONS: usize = 64;

/// Most concurrent connections that have not yet sent a request the token admits. When the pool
/// is full, the oldest is closed to make room, so sockets that hold a connection open without a
/// token cannot keep a new client out.
pub const MAX_PREAUTH_CONNECTIONS: usize = 128;

/// Most reads a request head may take: a head trickled a few bytes at a time is refused
/// (`400 head_too_fragmented`) however long its deadline. A real client sends a head in one or a
/// few segments (at most 16 KiB).
pub const MAX_HEAD_READS: usize = 64;

/// How long a new connection has to send its first request head (which carries the token).
const PREAUTH_HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a request waits for the app's answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a connection may take to send one request (head and body together; also the
/// longest idle wait between keep-alive requests).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A session unused this long is ended when a new session needs its slot.
const SESSION_IDLE: Duration = Duration::from_secs(10 * 60);

/// Buffer of the in-memory pipe between a session and its MCP server.
const PIPE_BYTES: usize = 1 << 20;

/// The token file name under `.regia/agent/`.
pub const TOKEN_FILE: &str = "mcp_http_token";

/// The advertisement file name under `.regia/agent/`.
pub const ADVERT_FILE: &str = "mcp_http.json";

/// Options of the HTTP transport.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// Where to listen.
    pub bind: SocketAddr,
    /// Allow a non-loopback bind.
    pub allow_remote: bool,
    /// Extra accepted `Origin` values.
    pub allowed_origins: Vec<String>,
    /// Most bytes of one request body.
    pub max_body: usize,
    /// An owner-provided token file (else a token is minted per launch).
    pub token_file: Option<PathBuf>,
}

/// One MCP session: the pipe to its server and the requests waiting for answers.
struct Session {
    to_server: Mutex<WriteHalf<DuplexStream>>,
    pending: StdMutex<HashMap<String, oneshot::Sender<Value>>>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    last_used: StdMutex<std::time::Instant>,
}

impl Session {
    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(PoisonError::into_inner) = std::time::Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }

    fn busy(&self) -> bool {
        !self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }
}

/// Frees a session slot when the table is full: sessions idle past [`SESSION_IDLE`] end, then
/// the least recently used session without a request in flight. Answers whether a slot is free.
fn make_room(sessions: &mut HashMap<String, Arc<Session>>) -> bool {
    if sessions.len() < MAX_SESSIONS {
        return true;
    }
    sessions.retain(|_, s| s.idle_for() < SESSION_IDLE || s.busy());
    if sessions.len() < MAX_SESSIONS {
        return true;
    }
    let oldest = sessions
        .iter()
        .filter(|(_, s)| !s.busy())
        .max_by_key(|(_, s)| s.idle_for())
        .map(|(id, _)| id.clone());
    if let Some(id) = oldest {
        sessions.remove(&id);
        tracing::info!(target: "regia_mcp", "ended the least recently used MCP session to open a new one");
    }
    sessions.len() < MAX_SESSIONS
}

impl Drop for Session {
    fn drop(&mut self) {
        for task in self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
    }
}

/// Shared server state.
struct State {
    policy: HttpPolicy,
    project_dir: PathBuf,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Connections not yet admitted, by accept order.
    pre_auth: StdMutex<PreAuth>,
    /// The [`MAX_CONNECTIONS`] slots of admitted connections.
    admitted: Arc<tokio::sync::Semaphore>,
}

/// The pre-auth pool: connections that have not yet sent a request the token admits, oldest
/// (lowest id) first.
#[derive(Default)]
struct PreAuth {
    next_id: u64,
    live: BTreeMap<u64, tokio::task::AbortHandle>,
}

impl PreAuth {
    /// Makes room for one more connection: closes the oldest when the pool holds `cap`. Answers
    /// whether one was closed.
    fn make_room(&mut self, cap: usize) -> bool {
        if self.live.len() < cap {
            return false;
        }
        match self.live.pop_first() {
            Some((_, oldest)) => {
                oldest.abort();
                true
            }
            None => false,
        }
    }
}

/// A connection's place in the pre-auth pool, given up when it is admitted or ends.
struct PreAuthSlot<'a> {
    state: &'a State,
    id: u64,
}

impl PreAuthSlot<'_> {
    /// Leaves the pool on admission. `false` when the connection was already evicted (its task is
    /// being aborted, so it must not take an admitted slot).
    fn leave(&self) -> bool {
        self.state
            .pre_auth
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live
            .remove(&self.id)
            .is_some()
    }
}

impl Drop for PreAuthSlot<'_> {
    fn drop(&mut self) {
        self.leave();
    }
}

/// The token for this launch: the owner's file, or a fresh one written owner-only.
fn launch_token(agent_dir: &Path, token_file: Option<&Path>) -> Result<(String, PathBuf)> {
    if let Some(file) = token_file {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file)
                .with_context(|| format!("cannot read the token file {}", file.display()))?
                .permissions()
                .mode();
            anyhow::ensure!(
                mode & 0o077 == 0,
                "the token file {} is readable by other users (mode {:o}); run chmod 600 on it",
                file.display(),
                mode & 0o777
            );
        }
        let token = std::fs::read_to_string(file)
            .with_context(|| format!("cannot read the token file {}", file.display()))?
            .trim()
            .to_string();
        anyhow::ensure!(
            token.len() >= 16 && token.bytes().all(|b| b.is_ascii_graphic()),
            "the token in {} must be at least 16 printable ASCII characters",
            file.display()
        );
        return Ok((token, file.to_path_buf()));
    }
    let token = regia_agent::endpoint::new_token();
    std::fs::create_dir_all(agent_dir).with_context(|| format!("cannot create {}", agent_dir.display()))?;
    regia_agent::endpoint::write_owner_only(agent_dir, TOKEN_FILE, token.as_bytes())
        .with_context(|| format!("cannot write {}", agent_dir.join(TOKEN_FILE).display()))?;
    Ok((token, agent_dir.join(TOKEN_FILE)))
}

/// What `regia mcp token` reads of a running server's advertisement ([`read_advert`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advert {
    /// The endpoint, `http://<loopback>:<port>/mcp`.
    pub url: String,
    /// The bearer token.
    pub token: String,
}

/// Largest advertisement or token file the helper reads.
const MAX_ADVERT_BYTES: u64 = 64 * 1024;

/// Whether `url` is the MCP endpoint on a loopback address: `http://127.0.0.1:<port>/mcp`,
/// `http://[::1]:<port>/mcp` or `http://localhost:<port>/mcp`, with a port of 1-65535.
pub fn is_loopback_mcp_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let Some(authority) = rest.strip_suffix(mcp_http::DEFAULT_PATH) else {
        return false;
    };
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    matches!(host, "127.0.0.1" | "[::1]" | "localhost")
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|p| p != 0)
}

/// A small regular file (not a symlink, at most `MAX_ADVERT_BYTES`), as text.
fn read_small_file(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    anyhow::ensure!(
        meta.file_type().is_file(),
        "{} is not a regular file (a link or another kind of file is not trusted)",
        path.display()
    );
    let mut text = String::new();
    std::fs::File::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
        .take(MAX_ADVERT_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("cannot read {}", path.display()))?;
    anyhow::ensure!(
        text.len() as u64 <= MAX_ADVERT_BYTES,
        "{} is larger than {MAX_ADVERT_BYTES} bytes",
        path.display()
    );
    Ok(text)
}

/// The URL and token of the `regia mcp --http` advertised in `project_dir`'s `.regia/agent/`,
/// checked the way `regia_agent::endpoint::read` checks the WebSocket advertisement (threat model
/// finding 31, ADR-3285). A project folder is not trusted: a cloned or shared one can hold a
/// planted `mcp_http.json` naming any file as the token and any host as the URL, and a client that
/// sends the one to the other leaks the file. So:
///
/// - the URL must be the MCP path on a loopback address ([`is_loopback_mcp_url`]);
/// - the token file must be the one the server mints, `.regia/agent/mcp_http_token` of this
///   project, a regular file (not a link), owner-only on Unix, holding at least 16 printable ASCII
///   characters. A server started with `--token-file` advertises the owner's own file, which this
///   helper refuses: its owner reads it directly.
///
/// # Errors
/// No advertisement, or one that fails a check above.
pub fn read_advert(project_dir: &Path) -> Result<Advert> {
    let agent_dir = regia_agent::endpoint::agent_dir(project_dir);
    let advert_path = agent_dir.join(ADVERT_FILE);
    let text = read_small_file(&advert_path)
        .context("no MCP HTTP server is advertised here (start `regia mcp --http`)")?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", advert_path.display()))?;
    let url = value["url"].as_str().unwrap_or_default().to_string();
    anyhow::ensure!(
        is_loopback_mcp_url(&url),
        "{}: `url` must be http://127.0.0.1:<port>/mcp (or [::1] or localhost); refusing {url:?}",
        advert_path.display()
    );
    let advertised = PathBuf::from(value["token_file"].as_str().unwrap_or_default());
    let minted = agent_dir.join(TOKEN_FILE);
    let agent_dir_real = agent_dir
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", agent_dir.display()))?;
    let same_file = advertised.file_name() == Some(std::ffi::OsStr::new(TOKEN_FILE))
        && advertised
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_some_and(|parent| parent == agent_dir_real);
    anyhow::ensure!(
        same_file,
        "{}: `token_file` must be {} (the token `regia mcp --http` mints); refusing {}. A server \
         started with --token-file uses your own file: read it directly",
        advert_path.display(),
        minted.display(),
        advertised.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&minted)
            .with_context(|| format!("cannot read {}", minted.display()))?
            .permissions()
            .mode();
        anyhow::ensure!(
            mode & 0o077 == 0,
            "{} is readable by other users (mode {:o}); it was not written by regia mcp --http",
            minted.display(),
            mode & 0o777
        );
    }
    let token = read_small_file(&minted)?.trim().to_string();
    anyhow::ensure!(
        token.len() >= 16 && token.bytes().all(|b| b.is_ascii_graphic()),
        "{} does not hold a token (at least 16 printable ASCII characters)",
        minted.display()
    );
    Ok(Advert { url, token })
}

/// Runs the HTTP transport until the process ends.
///
/// # Errors
/// A refused or failed bind, or an unreadable token file.
pub async fn serve(project_dir: PathBuf, options: &HttpOptions) -> Result<()> {
    let loopback = options.bind.ip().is_loopback();
    anyhow::ensure!(
        loopback || options.allow_remote,
        "refusing to listen on {} (not a loopback address) without --allow-remote; the bearer token \
         then guards the endpoint, so put it behind TLS",
        options.bind
    );
    let listener = TcpListener::bind(options.bind)
        .await
        .with_context(|| format!("cannot listen on {}", options.bind))?;
    let local = listener.local_addr()?;
    let agent_dir = regia_agent::endpoint::agent_dir(&project_dir);
    let (token, token_path) = launch_token(&agent_dir, options.token_file.as_deref())?;
    let url = format!("http://{local}{}", mcp_http::DEFAULT_PATH);
    let advert = json!({
        "url": url,
        "token_file": token_path.display().to_string(),
        "pid": std::process::id(),
        "loopback": loopback,
    });
    std::fs::create_dir_all(&agent_dir)?;
    regia_agent::endpoint::write_owner_only(
        &agent_dir,
        ADVERT_FILE,
        serde_json::to_string_pretty(&advert)?.as_bytes(),
    )
    .with_context(|| format!("cannot write {}", agent_dir.join(ADVERT_FILE).display()))?;
    if !loopback {
        tracing::warn!(target: "regia_mcp", "MCP over HTTP listens on {local}: reachable from the network");
    }
    eprintln!(
        "regia mcp: Streamable HTTP on {url} (token: {}; advertised in {})",
        token_path.display(),
        agent_dir.join(ADVERT_FILE).display()
    );
    let state = Arc::new(State {
        policy: HttpPolicy {
            token,
            path: mcp_http::DEFAULT_PATH.into(),
            loopback_bind: loopback,
            allowed_origins: options
                .allowed_origins
                .iter()
                .map(|o| o.trim_end_matches('/').to_ascii_lowercase())
                .collect(),
            max_body: options.max_body,
        },
        project_dir,
        sessions: Mutex::new(HashMap::new()),
        pre_auth: StdMutex::new(PreAuth::default()),
        admitted: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS)),
    });
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(target: "regia_mcp", "accept failed: {e}");
                continue;
            }
        };
        // The pool lock is held across the spawn, so the task cannot leave the pool before it is
        // entered in it.
        let mut pool = state.pre_auth.lock().unwrap_or_else(PoisonError::into_inner);
        if pool.make_room(MAX_PREAUTH_CONNECTIONS) {
            tracing::debug!(target: "regia_mcp", "closed the oldest unauthenticated connection to accept a new one");
        }
        let id = pool.next_id;
        pool.next_id += 1;
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            connection(stream, &task_state, id).await;
        });
        pool.live.insert(id, task.abort_handle());
        drop(pool);
    }
}

/// An HTTP response.
struct Reply {
    status: u16,
    body: String,
    session: Option<String>,
    close: bool,
}

impl Reply {
    fn refusal(refusal: &HttpRefusal) -> Self {
        Self {
            status: refusal.status,
            body: refusal.body(),
            session: None,
            close: true,
        }
    }
}

async fn write_reply(stream: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\n",
        reply.status,
        mcp_http::status_text(reply.status),
        reply.body.len()
    );
    if let Some(session) = &reply.session {
        head.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
    }
    if reply.status == 401 {
        head.push_str("WWW-Authenticate: Bearer\r\n");
    }
    if reply.status == 405 {
        head.push_str("Allow: POST, DELETE\r\n");
    }
    if reply.close {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(reply.body.as_bytes()).await?;
    stream.flush().await
}

async fn write_refusal(stream: &mut TcpStream, refusal: &HttpRefusal) -> std::io::Result<()> {
    write_reply(stream, &Reply::refusal(refusal)).await
}

/// Why [`read_head`] stopped without a head.
#[derive(Debug, PartialEq, Eq)]
enum HeadError {
    /// The peer closed, the read failed or the deadline passed: close without an answer.
    Closed,
    /// Over `MAX_HEAD_BYTES`.
    TooLarge,
    /// Over [`MAX_HEAD_READS`] reads.
    TooFragmented,
}

impl HeadError {
    /// The refusal to answer with, if any.
    fn refusal(&self) -> Option<HttpRefusal> {
        match self {
            Self::Closed => None,
            Self::TooLarge => Some(HttpRefusal {
                status: 431,
                reason: "head_too_large",
                message: format!("the request head is over {} bytes", mcp_http::MAX_HEAD_BYTES),
            }),
            Self::TooFragmented => Some(HttpRefusal {
                status: 400,
                reason: "head_too_fragmented",
                message: format!("the request head took more than {MAX_HEAD_READS} reads; send it whole"),
            }),
        }
    }
}

/// Reads from `reader` into `buffer` until it holds a whole request head, and answers where the
/// head ends. Bounded three ways: the head's size (`MAX_HEAD_BYTES`), the number of reads
/// ([`MAX_HEAD_READS`], counted, so a trickling client is cut off after a fixed number of polls
/// whatever the clock says) and `deadline`.
async fn read_head<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    deadline: tokio::time::Instant,
) -> Result<usize, HeadError> {
    let mut reads = 0usize;
    loop {
        if let Some(end) = mcp_http::head_end(buffer) {
            return Ok(end);
        }
        if buffer.len() > mcp_http::MAX_HEAD_BYTES {
            return Err(HeadError::TooLarge);
        }
        if reads >= MAX_HEAD_READS {
            return Err(HeadError::TooFragmented);
        }
        reads += 1;
        let mut chunk = [0u8; 8192];
        match tokio::time::timeout_at(deadline, reader.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return Err(HeadError::Closed),
            Ok(Ok(n)) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Serves one connection: requests in order, keep-alive until the client closes or a refusal.
/// The connection is pre-auth (in the pool under `id`) until a request passes the token check;
/// then it leaves the pool and takes one of the [`MAX_CONNECTIONS`] admitted slots for the rest of
/// its life.
async fn connection(mut stream: TcpStream, state: &State, id: u64) {
    let pre_auth = PreAuthSlot { state, id };
    let mut admitted_slot: Option<tokio::sync::OwnedSemaphorePermit> = None;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        // One deadline per request: a client trickling bytes cannot hold the connection longer.
        // The first head, before the token is known, has a shorter one of its own.
        let started = tokio::time::Instant::now();
        let deadline = started + READ_TIMEOUT;
        let head_deadline = if admitted_slot.is_some() {
            deadline
        } else {
            started + PREAUTH_HEAD_TIMEOUT
        };
        let end = match read_head(&mut stream, &mut buffer, head_deadline).await {
            Ok(end) => end,
            Err(error) => {
                if let Some(refusal) = error.refusal() {
                    let _ = write_refusal(&mut stream, &refusal).await;
                }
                return;
            }
        };
        let head = match mcp_http::parse_head(&buffer[..end]) {
            Ok(head) => head,
            Err(refusal) => {
                let _ = write_refusal(&mut stream, &refusal).await;
                return;
            }
        };
        buffer.drain(..end);
        let admitted = match mcp_http::admit(&state.policy, &head) {
            Ok(admitted) => admitted,
            Err(refusal) => {
                let _ = write_refusal(&mut stream, &refusal).await;
                return;
            }
        };
        if admitted_slot.is_none() {
            if !pre_auth.leave() {
                // Evicted from the pool while this head was judged; the task is being aborted.
                return;
            }
            match state.admitted.clone().try_acquire_owned() {
                Ok(permit) => admitted_slot = Some(permit),
                Err(_) => {
                    let refusal = HttpRefusal {
                        status: 503,
                        reason: "too_many_connections",
                        message: format!("at most {MAX_CONNECTIONS} connections"),
                    };
                    let _ = write_refusal(&mut stream, &refusal).await;
                    return;
                }
            }
        }
        let mut reply = match admitted {
            Admitted::Delete => end_session(state, &head).await,
            Admitted::Post { body_len } => {
                while buffer.len() < body_len {
                    let mut chunk = vec![0u8; (body_len - buffer.len()).min(64 * 1024)];
                    match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
                        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
                        Ok(Ok(n)) => buffer.extend_from_slice(&chunk[..n]),
                    }
                }
                let body: Vec<u8> = buffer.drain(..body_len).collect();
                post(state, &head, &body).await
            }
        };
        reply.close |= head.wants_close();
        if write_reply(&mut stream, &reply).await.is_err() || reply.close {
            return;
        }
    }
}

fn refused(status: u16, reason: &'static str, message: impl Into<String>) -> Reply {
    Reply::refusal(&HttpRefusal {
        status,
        reason,
        message: message.into(),
    })
}

fn ok_reply(status: u16, body: String, session: Option<String>) -> Reply {
    Reply {
        status,
        body,
        session,
        close: false,
    }
}

async fn lookup(state: &State, head: &RequestHead) -> Result<Arc<Session>, Reply> {
    let Some(id) = head.header(mcp_http::SESSION_HEADER) else {
        return Err(refused(
            400,
            "session_required",
            "send the Mcp-Session-Id that initialize answered",
        ));
    };
    let session = state.sessions.lock().await.get(id).cloned().ok_or_else(|| {
        refused(
            404,
            "session_not_found",
            "unknown or ended session: initialize again",
        )
    })?;
    session.touch();
    Ok(session)
}

async fn end_session(state: &State, head: &RequestHead) -> Reply {
    let Some(id) = head.header(mcp_http::SESSION_HEADER) else {
        return refused(
            400,
            "session_required",
            "name the session to end in Mcp-Session-Id",
        );
    };
    match state.sessions.lock().await.remove(id) {
        Some(_) => ok_reply(200, "{}".into(), None),
        None => refused(404, "session_not_found", "unknown or ended session"),
    }
}

/// Starts a session: an MCP server on one end of a pipe, a reader routing its answers on the other.
fn open_session(project_dir: &Path) -> Arc<Session> {
    let (client_side, server_side) = tokio::io::duplex(PIPE_BYTES);
    let (server_read, server_write) = tokio::io::split(server_side);
    let (client_read, client_write) = tokio::io::split(client_side);
    let session = Arc::new(Session {
        to_server: Mutex::new(client_write),
        pending: StdMutex::new(HashMap::new()),
        tasks: StdMutex::new(Vec::new()),
        last_used: StdMutex::new(std::time::Instant::now()),
    });
    let server = super::mcp::McpServer::new(project_dir.to_path_buf());
    let serve = tokio::spawn(async move {
        match rmcp::serve_server(server, (server_read, server_write)).await {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(e) => tracing::warn!(target: "regia_mcp", "MCP session ended at start: {e}"),
        }
    });
    let weak = Arc::downgrade(&session);
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(client_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let is_answer = message.get("result").is_some() || message.get("error").is_some();
            let Some(id) = message.get("id").filter(|_| is_answer).map(Value::to_string) else {
                // A server notification or request: no stream to send it on.
                continue;
            };
            let Some(session) = weak.upgrade() else { break };
            let waiter = session
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(message);
            }
        }
    });
    session
        .tasks
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .extend([serve, reader]);
    session
}

async fn send_line(session: &Session, message: &Value) -> std::io::Result<()> {
    let mut line = message.to_string();
    line.push('\n');
    let mut pipe = session.to_server.lock().await;
    pipe.write_all(line.as_bytes()).await?;
    pipe.flush().await
}

/// Handles one POSTed message.
async fn post(state: &State, head: &RequestHead, body: &[u8]) -> Reply {
    let (kind, message) = match mcp_http::classify_body(body) {
        Ok(classified) => classified,
        Err(refusal) => return Reply::refusal(&refusal),
    };
    match kind {
        BodyKind::Request { id, initialize } => {
            let (session, session_id) = if initialize {
                if head.header(mcp_http::SESSION_HEADER).is_some() {
                    return refused(
                        400,
                        "session_on_initialize",
                        "initialize opens a session; do not send one",
                    );
                }
                let mut sessions = state.sessions.lock().await;
                if !make_room(&mut sessions) {
                    return refused(
                        503,
                        "too_many_sessions",
                        format!("at most {MAX_SESSIONS} sessions; DELETE unused ones"),
                    );
                }
                let id = regia_agent::endpoint::new_token();
                let session = open_session(&state.project_dir);
                sessions.insert(id.clone(), session.clone());
                (session, Some(id))
            } else {
                match lookup(state, head).await {
                    Ok(session) => (session, None),
                    Err(reply) => return reply,
                }
            };
            let key = id.to_string();
            let (tx, rx) = oneshot::channel();
            {
                let mut pending = session.pending.lock().unwrap_or_else(PoisonError::into_inner);
                if pending.contains_key(&key) {
                    return refused(
                        409,
                        "id_in_use",
                        format!("request id {key} is still being answered"),
                    );
                }
                pending.insert(key.clone(), tx);
            }
            if send_line(&session, &message).await.is_err() {
                session
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&key);
                return refused(
                    500,
                    "session_broken",
                    "the session's MCP server stopped: initialize again",
                );
            }
            let answered = tokio::time::timeout(REPLY_TIMEOUT, rx).await;
            // A failed initialize never hands its session id out: end the session now.
            let opened_ok = matches!(&answered, Ok(Ok(answer)) if answer.get("error").is_none());
            if let Some(opened) = &session_id
                && !opened_ok
            {
                state.sessions.lock().await.remove(opened);
            }
            match answered {
                Ok(Ok(answer)) => ok_reply(200, answer.to_string(), session_id.filter(|_| opened_ok)),
                Ok(Err(_)) => refused(
                    500,
                    "session_broken",
                    "the session's MCP server stopped: initialize again",
                ),
                Err(_) => {
                    session
                        .pending
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&key);
                    refused(504, "timeout", "no answer in time")
                }
            }
        }
        BodyKind::Notification | BodyKind::Response => {
            let session = match lookup(state, head).await {
                Ok(session) => session,
                Err(reply) => return reply,
            };
            if send_line(&session, &message).await.is_err() {
                return refused(
                    500,
                    "session_broken",
                    "the session's MCP server stopped: initialize again",
                );
            }
            ok_reply(202, String::new(), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// A reader that hands out `step` bytes per read.
    struct Trickle {
        data: Vec<u8>,
        at: usize,
        step: usize,
        reads: usize,
    }

    impl AsyncRead for Trickle {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let end = (self.at + self.step)
                .min(self.data.len())
                .min(self.at + buf.remaining());
            let at = self.at;
            buf.put_slice(&self.data[at..end]);
            self.at = end;
            self.reads += 1;
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn trickle(head: &str, step: usize) -> Trickle {
        Trickle {
            data: head.as_bytes().to_vec(),
            at: 0,
            step,
            reads: 0,
        }
    }

    const HEAD: &str = "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer 0123456789abcdef\r\n\
                        Content-Type: application/json\r\nContent-Length: 2\r\n\r\n";

    /// Threat model finding 24: a head trickled a byte per read is cut off after a counted number
    /// of reads, not by a clock; the same head sent whole or in a few pieces is read.
    #[test]
    fn a_trickled_head_is_refused_after_a_counted_number_of_reads() {
        let rt = runtime();
        // A deadline far away: only the read count can stop the trickle.
        let far = tokio::time::Instant::now() + Duration::from_secs(3600);
        rt.block_on(async {
            assert!(HEAD.len() > MAX_HEAD_READS);
            let mut slow = trickle(HEAD, 1);
            let mut buffer = Vec::new();
            assert_eq!(
                read_head(&mut slow, &mut buffer, far).await,
                Err(HeadError::TooFragmented)
            );
            assert_eq!(slow.reads, MAX_HEAD_READS);
            for step in [HEAD.len(), 40, 4] {
                let mut reader = trickle(HEAD, step);
                let mut buffer = Vec::new();
                assert_eq!(
                    read_head(&mut reader, &mut buffer, far).await,
                    Ok(HEAD.len()),
                    "{step} bytes a read"
                );
            }
            let mut closed = trickle("POST /mcp HTTP/1.1\r\n", 8192);
            let mut buffer = Vec::new();
            assert_eq!(
                read_head(&mut closed, &mut buffer, far).await,
                Err(HeadError::Closed)
            );
            // No blank line yet and over the size: refused before it ends.
            let big = format!(
                "POST /mcp HTTP/1.1\r\nX: {}",
                "a".repeat(mcp_http::MAX_HEAD_BYTES + 100)
            );
            let mut reader = trickle(&big, 8192);
            let mut buffer = Vec::new();
            assert_eq!(
                read_head(&mut reader, &mut buffer, far).await,
                Err(HeadError::TooLarge)
            );
        });
        assert_eq!(
            HeadError::TooFragmented.refusal().map(|r| (r.status, r.reason)),
            Some((400, "head_too_fragmented"))
        );
        assert!(HeadError::Closed.refusal().is_none());
    }

    /// Threat model finding 24: a full pre-auth pool closes its oldest connection for a newcomer,
    /// and a connection evicted while its head was judged cannot take an admitted slot.
    #[test]
    fn a_full_pre_auth_pool_closes_its_oldest_connection() {
        let rt = runtime();
        rt.block_on(async {
            let mut pool = PreAuth::default();
            let mut tasks = Vec::new();
            for id in 0..3u64 {
                let task = tokio::spawn(std::future::pending::<()>());
                pool.live.insert(id, task.abort_handle());
                tasks.push(task);
            }
            assert!(!pool.make_room(4), "room left");
            assert!(pool.make_room(3), "full: the oldest goes");
            assert_eq!(pool.live.keys().copied().collect::<Vec<_>>(), [1, 2]);
            let oldest = tasks.remove(0).await;
            assert!(
                oldest.is_err_and(|e| e.is_cancelled()),
                "the oldest task was aborted"
            );
            for task in &tasks {
                assert!(!task.is_finished());
            }
            for task in tasks {
                task.abort();
            }
        });
    }

    fn advert_project(tag: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(regia_agent::endpoint::agent_dir(&dir)).expect("agent dir");
        dir
    }

    fn write_advert(project: &Path, url: &str, token_file: &Path) {
        let agent = regia_agent::endpoint::agent_dir(project);
        let text =
            json!({ "url": url, "token_file": token_file.display().to_string(), "pid": 1, "loopback": true });
        std::fs::write(agent.join(ADVERT_FILE), text.to_string()).expect("advert");
    }

    /// Threat model finding 31: the advertisement is checked before its token file is read.
    #[test]
    fn the_token_helper_refuses_a_planted_advert() {
        let project = advert_project("mcp-advert");
        let agent = regia_agent::endpoint::agent_dir(&project);
        let (token, minted) = launch_token(&agent, None).expect("minted");
        write_advert(&project, "http://127.0.0.1:8931/mcp", &minted);
        assert_eq!(
            read_advert(&project).expect("a good advert"),
            Advert {
                url: "http://127.0.0.1:8931/mcp".into(),
                token: token.clone()
            }
        );
        // Any file as the token, any host as the URL: refused before the file is read.
        let secret = project.join("secret.txt");
        std::fs::write(&secret, "0123456789abcdef-SECRET").expect("secret");
        write_advert(&project, "http://127.0.0.1:8931/mcp", &secret);
        let error = read_advert(&project)
            .expect_err("a token file outside .regia/agent")
            .to_string();
        assert!(!error.contains("SECRET"), "{error}");
        for url in [
            "http://evil.example:80/mcp",
            "https://127.0.0.1:8931/mcp",
            "http://127.0.0.1:8931/other",
            "http://127.0.0.1.evil.example:8931/mcp",
            "http://127.0.0.1:0/mcp",
            "http://user@127.0.0.1:8931/mcp",
        ] {
            write_advert(&project, url, &minted);
            assert!(read_advert(&project).is_err(), "{url}");
        }
        for url in ["http://[::1]:8931/mcp", "http://localhost:8931/mcp"] {
            write_advert(&project, url, &minted);
            assert!(read_advert(&project).is_ok(), "{url}");
        }
        // The minted name in another folder, and a link in place of the minted file.
        let elsewhere = project.join("other");
        std::fs::create_dir_all(&elsewhere).expect("dir");
        std::fs::write(elsewhere.join(TOKEN_FILE), &token).expect("copy");
        write_advert(&project, "http://127.0.0.1:8931/mcp", &elsewhere.join(TOKEN_FILE));
        assert!(
            read_advert(&project).is_err(),
            "the minted name in another folder"
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(&minted).expect("remove");
            std::os::unix::fs::symlink(&secret, &minted).expect("symlink");
            write_advert(&project, "http://127.0.0.1:8931/mcp", &minted);
            let error = read_advert(&project)
                .expect_err("a link to another file")
                .to_string();
            assert!(!error.contains("SECRET"), "{error}");
        }
        assert!(read_advert(&project.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_minted_token_is_owner_only_and_a_short_owner_token_is_refused() {
        let dir = regia_test_utils::unique_temp_dir("mcp-http-token");
        let (token, path) = launch_token(&dir, None).expect("minted");
        assert!(token.len() >= 16);
        assert_eq!(std::fs::read_to_string(&path).expect("written"), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let short = dir.join("short");
        std::fs::write(&short, "abc\n").expect("short token");
        assert!(launch_token(&dir, Some(&short)).is_err());
        let good = dir.join("good");
        std::fs::write(&good, "0123456789abcdef0123\n").expect("good token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o644)).expect("chmod");
            let error = launch_token(&dir, Some(&good)).expect_err("a world-readable token file");
            assert!(error.to_string().contains("chmod 600"), "{error}");
            std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        assert_eq!(
            launch_token(&dir, Some(&good)).expect("read").0,
            "0123456789abcdef0123"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn idle_session(idle: Duration, busy: bool) -> Arc<Session> {
        let (pipe, _) = tokio::io::duplex(64);
        let (_, write) = tokio::io::split(pipe);
        let now = std::time::Instant::now();
        let session = Session {
            to_server: Mutex::new(write),
            pending: StdMutex::new(HashMap::new()),
            tasks: StdMutex::new(Vec::new()),
            last_used: StdMutex::new(now.checked_sub(idle).unwrap_or(now)),
        };
        if busy {
            let (tx, _) = oneshot::channel();
            session
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert("1".into(), tx);
        }
        Arc::new(session)
    }

    #[test]
    fn a_full_session_table_ends_idle_then_least_recently_used_sessions() {
        let mut sessions = HashMap::new();
        for n in 0..MAX_SESSIONS {
            let idle = if n == 3 {
                SESSION_IDLE * 2
            } else {
                Duration::from_secs(n as u64)
            };
            sessions.insert(format!("s{n}"), idle_session(idle, false));
        }
        assert!(make_room(&mut sessions));
        assert!(!sessions.contains_key("s3"), "the idle session ended");
        // Full again with no idle session: the least recently used one without a request in
        // flight makes way; a busy one never does.
        sessions.insert("s3".into(), idle_session(Duration::from_secs(1), false));
        let last = format!("s{}", MAX_SESSIONS - 1);
        sessions.insert(last.clone(), idle_session(Duration::from_secs(500), true));
        assert!(make_room(&mut sessions));
        assert!(sessions.contains_key(&last), "a busy session stays");
        assert!(!sessions.contains_key(&format!("s{}", MAX_SESSIONS - 2)));
        // Every session busy: no room.
        let mut busy: HashMap<String, Arc<Session>> = (0..MAX_SESSIONS)
            .map(|n| (format!("b{n}"), idle_session(Duration::ZERO, true)))
            .collect();
        assert!(!make_room(&mut busy));
    }
}
