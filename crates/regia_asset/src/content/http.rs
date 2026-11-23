// SPDX-License-Identifier: MPL-2.0
//! The HTTP chunk source and a small static file server (spec 40 § 2.6, § 5).
//!
//! [`HttpSource`] fetches `<base_url>/<build_id>/<file>`, resuming from an existing `.part` file
//! with a `Range` request. `http://` goes through a dependency-free HTTP/1.1 client over
//! `std::net`; `https://` goes through the approved `ureq` + rustls stack (`ring` provider, web PKI
//! roots) behind the optional `content-http` feature (ADR-1876). Without the feature an `https://`
//! base URL is refused with `source_not_supported`. Plain `http://` is for this machine only: a
//! non-loopback `http://` host is refused with `source_not_supported` unless the developer override
//! [`CONTENT_ALLOW_HTTP_ENV`] is set or the client was built with
//! [`HttpClient::allow_plain_http`] (audit v0.2.12; ADR-2138 proposes signed manifests). Each transfer runs on its own thread (a
//! transfer belongs to a plain thread, not to a frame or a surface) and [`Transfer::step`] only
//! reads its progress.
//!
//! [`serve_dir`] is the test and tooling server behind `regia content serve`: `GET` only, `Range`
//! (`bytes=N-` and `bytes=N-M`), no directory listing, no path outside its root. Not for
//! production.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::manifest::ChunkEntry;
use super::source::{ChunkSource, PartLease, SourceStatus, Transfer, TransferStep, resume_point};

/// Socket read/write timeout of a transfer (a stalled host fails the transfer, it does not hang it).
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest response header block accepted.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Stall rule (spec 40 § 2.6): a transfer must move this many bytes in each window of
/// [`STALL_WINDOW_MS`], or it fails. The socket timeout alone lets one byte every 29 s hold a
/// transfer thread for as long as the host likes.
const STALL_MIN_BYTES: u64 = 4096;
/// Length of a stall window.
const STALL_WINDOW_MS: u64 = 60_000;
/// Longest a whole response header block may take to arrive.
const HEADER_DEADLINE: Duration = Duration::from_secs(60);

/// Environment variable (`1`/`true`) that lets plain `http://` reach a host other than this
/// machine for one run: a developer override for a LAN test server (`regia content serve` on a
/// workstation, a phone on the same network). Never set it for players: nothing authenticates
/// plain `http://` content.
pub const CONTENT_ALLOW_HTTP_ENV: &str = "REGIA_CONTENT_ALLOW_HTTP";

/// Whether [`CONTENT_ALLOW_HTTP_ENV`] is set to `1` or `true` for this run.
#[must_use]
pub fn plain_http_allowed_by_env() -> bool {
    regia_core::brand::env_var(CONTENT_ALLOW_HTTP_ENV)
        .is_ok_and(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"))
}

/// A parsed `http://host[:port]/path` or `https://host[:port]/path` URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpUrl {
    /// `https://` (TLS through the `content-http` feature) rather than `http://`.
    pub https: bool,
    /// Host name or address.
    pub host: String,
    /// Port (80 for `http://`, 443 for `https://` by default).
    pub port: u16,
    /// Path with a leading `/`, without a trailing `/`.
    pub path: String,
}

impl HttpUrl {
    /// Parses an `http://` or `https://` URL. Whether `https://` can be fetched depends on the
    /// `content-http` feature ([`HttpClient::supports_https`]).
    ///
    /// # Errors
    /// Another scheme, an empty host, or a bad port.
    pub fn parse(url: &str) -> Result<Self, String> {
        let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(format!("{url:?} is not an http:// or https:// URL"));
        };
        let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
        let default_port = if https { 443 } else { 80 };
        let bad_port = || format!("bad port in {url:?}");
        // `[v6]` or `[v6]:port` (audit v0.2.12: the last `:` of the address was taken as the port).
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (v6, after) = bracketed
                .split_once(']')
                .ok_or_else(|| format!("bad host in {url:?}"))?;
            if v6.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(format!("bad host in {url:?}"));
            }
            let port = match after {
                "" => default_port,
                p => p
                    .strip_prefix(':')
                    .and_then(|p| p.parse::<u16>().ok())
                    .ok_or_else(bad_port)?,
            };
            (v6, port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse::<u16>().map_err(|_| bad_port())?),
                None => (authority, default_port),
            }
        };
        // No user info: `user@host` would send the name to whatever `host` resolves.
        if host.is_empty() || host.contains(['/', ' ', '\r', '\n', '@', '[', ']']) {
            return Err(format!("bad host in {url:?}"));
        }
        if path.contains(['\r', '\n', ' ']) {
            return Err(format!("bad path in {url:?}"));
        }
        let path = format!("/{}", path.trim_end_matches('/'));
        Ok(Self {
            https,
            host: host.to_owned(),
            port,
            path: if path == "/" { String::new() } else { path },
        })
    }

    /// Whether the host is this machine's loopback interface (`localhost`, `127.0.0.0/8`, `::1`).
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }

    /// The host as it goes in a URL or a `Host` header: an IPv6 address in brackets.
    #[must_use]
    pub fn host_for_url(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// The scheme, host and port: `https://cdn.example.com:443`.
    #[must_use]
    pub fn origin(&self) -> String {
        format!(
            "{}://{}:{}",
            if self.https { "https" } else { "http" },
            self.host_for_url(),
            self.port
        )
    }
}

/// How [`HttpSource`] and [`fetch_manifest_with`] talk to a host: the built-in `std::net` client
/// for `http://`, and (with the `content-http` feature) a `ureq` agent with rustls for `https://`.
#[derive(Clone, Default)]
pub struct HttpClient {
    #[cfg(feature = "content-http")]
    agent: Option<ureq::Agent>,
    /// Plain `http://` to a non-loopback host is allowed (developer override).
    allow_http: bool,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("https", &Self::supports_https())
            .field("allow_plain_http", &self.allow_http)
            .finish()
    }
}

impl HttpClient {
    /// The default client: `https://` with the `content-http` feature (web PKI roots, 30 s
    /// connect/read/write timeouts, at most 5 redirects, none of them to `http://`); plain
    /// `http://` to loopback hosts only, unless [`CONTENT_ALLOW_HTTP_ENV`] is set for this run.
    #[must_use]
    pub fn new() -> Self {
        Self::default().allow_plain_http(plain_http_allowed_by_env())
    }

    /// This client, allowing (or not) plain `http://` to hosts other than this machine. A tool or a
    /// test host may opt in; a shipped game should not (nothing authenticates the bytes).
    #[must_use]
    pub fn allow_plain_http(mut self, allow: bool) -> Self {
        self.allow_http = allow;
        self
    }

    /// Whether this build can fetch `https://` URLs (the `content-http` feature).
    #[must_use]
    pub fn supports_https() -> bool {
        cfg!(feature = "content-http")
    }

    /// A client whose `https://` requests go through `agent` (tests trust a self-signed
    /// certificate this way; hosts may pin their own roots).
    #[cfg(feature = "content-http")]
    #[must_use]
    pub fn with_agent(agent: ureq::Agent) -> Self {
        Self {
            agent: Some(agent),
            allow_http: plain_http_allowed_by_env(),
        }
    }

    /// Refuses a URL this client cannot or may not fetch: `https://` without the feature, and plain
    /// `http://` to a host other than this machine without the developer override.
    fn check(&self, url: &HttpUrl) -> Result<(), String> {
        if url.https && !Self::supports_https() {
            return Err(
                "https:// needs the content-http feature (regia_asset/content-http, or regia's content-http)"
                    .into(),
            );
        }
        if !url.https && !url.is_loopback() && !self.allow_http {
            return Err(format!(
                "plain http:// to {} is refused: content from another machine must use https:// \
                 (set {CONTENT_ALLOW_HTTP_ENV}=1 to allow it for development)",
                url.host
            ));
        }
        Ok(())
    }

    /// Sends `GET path` with `Range: bytes=<start>-` (when `start > 0`) and returns the reply.
    fn get(&self, url: &HttpUrl, path: &str, start: u64) -> Result<Reply, String> {
        self.check(url)?;
        if url.https {
            return self.get_tls(url, path, start);
        }
        request(url, path, start)
    }

    #[cfg(feature = "content-http")]
    fn get_tls(&self, url: &HttpUrl, path: &str, start: u64) -> Result<Reply, String> {
        static DEFAULT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
        let agent = self.agent.as_ref().unwrap_or_else(|| {
            DEFAULT.get_or_init(|| {
                ureq::AgentBuilder::new()
                    .timeout_connect(IO_TIMEOUT)
                    .timeout_read(IO_TIMEOUT)
                    .timeout_write(IO_TIMEOUT)
                    .user_agent("regia-content")
                    // A redirect must not take a manifest or a chunk back to plain `http://`.
                    .https_only(true)
                    .build()
            })
        });
        let mut req = agent.get(&format!("{}{path}", url.origin()));
        if start > 0 {
            req = req.set("Range", &format!("bytes={start}-"));
        }
        let response = match req.call() {
            Ok(response) => response,
            // 4xx/5xx: the caller maps the status.
            Err(ureq::Error::Status(code, _)) => {
                return Ok(Reply {
                    status: code,
                    body: Box::new(std::io::empty()),
                    length: Some(0),
                    range_start: None,
                });
            }
            Err(ureq::Error::Transport(t)) => return Err(format!("{}: {t}", url.host)),
        };
        let status = response.status();
        let length = response
            .header("Content-Length")
            .and_then(|v| v.trim().parse::<u64>().ok());
        let range_start = response.header("Content-Range").and_then(content_range_start);
        Ok(Reply {
            status,
            body: Box::new(response.into_reader()),
            length,
            range_start,
        })
    }

    #[cfg(not(feature = "content-http"))]
    fn get_tls(&self, _url: &HttpUrl, _path: &str, _start: u64) -> Result<Reply, String> {
        Err("https:// needs the content-http feature".into())
    }
}

/// Fetches chunks from a static file host: `<base_url>/<build_id>/<file>`.
#[derive(Clone, Debug)]
pub struct HttpSource {
    url: HttpUrl,
    build_id: String,
    client: HttpClient,
}

impl HttpSource {
    /// A source for `base_url` serving files of build `build_id`, with the default client.
    ///
    /// # Errors
    /// The URL is not a valid `http://` URL, or it is `https://` and this build has no
    /// `content-http` feature, or it is plain `http://` to another machine without the developer
    /// override ([`HttpClient::new`]), or `build_id` fails [`super::validate_build_id`].
    pub fn new(base_url: &str, build_id: &str) -> Result<Self, String> {
        Self::with_client(base_url, build_id, HttpClient::new())
    }

    /// Like [`HttpSource::new`] with a caller's client (a pinned or test TLS configuration).
    ///
    /// # Errors
    /// As [`HttpSource::new`].
    pub fn with_client(base_url: &str, build_id: &str, client: HttpClient) -> Result<Self, String> {
        let url = HttpUrl::parse(base_url)?;
        client.check(&url)?;
        // The id is a path segment of every request line (audit v0.2.12: a CR/LF split it).
        super::manifest::validate_build_id(build_id).map_err(|why| format!("build id {build_id:?} {why}"))?;
        Ok(Self {
            url,
            build_id: build_id.to_owned(),
            client,
        })
    }

    fn file_path(&self, file: &str) -> String {
        format!("{}/{}/{}", self.url.path, self.build_id, file)
    }
}

impl ChunkSource for HttpSource {
    fn name(&self) -> &'static str {
        "http"
    }

    fn status(&self, _chunk: &ChunkEntry) -> SourceStatus {
        // Asking the host would block a frame; a missing file fails the transfer instead.
        SourceStatus::Available
    }

    fn start(&self, chunk: &ChunkEntry, dest: &Path) -> Box<dyn Transfer> {
        let shared = Arc::new(Shared::default());
        shared.total.store(chunk.size, Ordering::Relaxed);
        let job = Job {
            client: self.client.clone(),
            url: self.url.clone(),
            path: self.file_path(&chunk.file),
            dest: dest.to_path_buf(),
            total: chunk.size,
            blocks: chunk.blocks.clone(),
            shared: Arc::clone(&shared),
        };
        let handle = std::thread::Builder::new()
            .name(format!("content-http-{}", chunk.name))
            .spawn(move || {
                // A panic must still finish the transfer, or `step` reports progress forever and
                // the chunk never ends (audit concurrency-1103 M2).
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run()))
                    .unwrap_or_else(|_| Err("the transfer thread panicked".into()));
                if let Err(why) = outcome {
                    *job.shared.error.lock() = Some(why);
                }
                job.shared.finished.store(true, Ordering::Release);
            });
        let handle = match handle {
            Ok(handle) => Some(handle),
            Err(e) => {
                *shared.error.lock() = Some(format!("cannot start the transfer thread: {e}"));
                shared.finished.store(true, Ordering::Release);
                None
            }
        };
        Box::new(HttpTransfer { shared, handle })
    }
}

#[derive(Default)]
struct Shared {
    done: AtomicU64,
    total: AtomicU64,
    finished: AtomicBool,
    cancel: AtomicBool,
    error: Mutex<Option<String>>,
}

struct HttpTransfer {
    shared: Arc<Shared>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HttpTransfer {
    fn drop(&mut self) {
        // Pause, cancel or release: the thread stops at its next read and keeps the `.part`.
        self.shared.cancel.store(true, Ordering::Release);
    }
}

impl Transfer for HttpTransfer {
    fn wait(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn step(&mut self, _budget: u64) -> TransferStep {
        let done = self.shared.done.load(Ordering::Relaxed);
        let total = self.shared.total.load(Ordering::Relaxed);
        if !self.shared.finished.load(Ordering::Acquire) {
            return TransferStep::Progress { done, total };
        }
        match self.shared.error.lock().clone() {
            Some(why) => TransferStep::Failed(why),
            None => TransferStep::Done,
        }
    }
}

/// [`STALL_MIN_BYTES`] per [`STALL_WINDOW_MS`], fed with a time so a test needs no real clock.
#[derive(Debug, Default)]
struct StallGuard {
    window_start_ms: u64,
    window_bytes: u64,
}

impl StallGuard {
    /// Counts `bytes` arriving at `now_ms`; `false` when a whole window passed below the minimum.
    fn note(&mut self, now_ms: u64, bytes: u64) -> bool {
        self.window_bytes = self.window_bytes.saturating_add(bytes);
        if now_ms.saturating_sub(self.window_start_ms) < STALL_WINDOW_MS {
            return true;
        }
        let moving = self.window_bytes >= STALL_MIN_BYTES;
        self.window_start_ms = now_ms;
        self.window_bytes = 0;
        moving
    }
}

struct Job {
    client: HttpClient,
    url: HttpUrl,
    path: String,
    dest: PathBuf,
    total: u64,
    blocks: Vec<String>,
    shared: Arc<Shared>,
}

impl Job {
    fn run(&self) -> Result<(), String> {
        let _span =
            tracing::info_span!(target: "regia_asset", "content_transfer", path = %self.path).entered();
        if let Some(parent) = self.dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let Some(_lease) = PartLease::acquire(&self.dest, &self.shared.cancel) else {
            return Err("cancelled".into());
        };
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.dest)
            .map_err(|e| format!("{}: {e}", self.dest.display()))?;
        let len = out.metadata().map_err(|e| e.to_string())?.len().min(self.total);
        let start = resume_point(&mut out, len, &self.blocks).map_err(|e| e.to_string())?;
        self.shared.done.store(start, Ordering::Relaxed);
        if start >= self.total {
            out.set_len(self.total).map_err(|e| e.to_string())?;
            return Ok(());
        }
        let mut from = start;
        let mut reply = self.client.get(&self.url, &self.path, from)?;
        if reply.status == 206 && reply.range_start != Some(from) {
            // A 206 for other bytes would write them at the wrong offset and fail the final hash on
            // every retry, reported as corruption instead of a host that ignores ranges.
            from = 0;
            reply = self.client.get(&self.url, &self.path, 0)?;
        }
        let Reply {
            status,
            mut body,
            length,
            range_start,
        } = reply;
        let offset = match status {
            206 if range_start == Some(from) => from,
            206 => return Err(format!("{} answered with another range", self.url.host)),
            200 => 0, // the host ignored the range: start over
            404 => return Err(format!("{} not found on {}", self.path, self.url.host)),
            other => return Err(format!("HTTP {other} for {}", self.path)),
        };
        out.set_len(offset).map_err(|e| e.to_string())?;
        out.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        self.shared.done.store(offset, Ordering::Relaxed);
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = offset;
        let mut remaining = length.unwrap_or(u64::MAX);
        let started = Instant::now();
        let mut stall = StallGuard::default();
        while done < self.total && remaining > 0 {
            if self.shared.cancel.load(Ordering::Acquire) {
                let _ = out.sync_all();
                return Err("cancelled".into());
            }
            // Never read past the manifest size: a longer body must not leave extra bytes.
            let cap = remaining.min(self.total - done);
            let want = usize::try_from(cap.min(buf.len() as u64)).unwrap_or(buf.len());
            let n = body.read(&mut buf[..want]).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            // A read can block for a whole timeout: a pause or cancel that landed meanwhile drops
            // these bytes instead of writing them (audit concurrency-1103 H1).
            if self.shared.cancel.load(Ordering::Acquire) {
                let _ = out.sync_all();
                return Err("cancelled".into());
            }
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            done += n as u64;
            remaining = remaining.saturating_sub(n as u64);
            self.shared.done.store(done, Ordering::Relaxed);
            let now_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            if !stall.note(now_ms, n as u64) {
                let _ = out.sync_all();
                return Err(format!("{} is stalled on {}", self.path, self.url.host));
            }
        }
        out.sync_all().map_err(|e| e.to_string())?;
        if done < self.total {
            return Err(format!(
                "{} ended after {done} of {} bytes",
                self.path, self.total
            ));
        }
        Ok(())
    }
}

/// What a `GET` answered.
struct Reply {
    status: u16,
    body: Box<dyn Read + Send>,
    /// `Content-Length`.
    length: Option<u64>,
    /// First byte of the `Content-Range`.
    range_start: Option<u64>,
}

/// Reads one line of a header block into `line`, never more than `left + 1` bytes of it and never
/// past `deadline`: `read_line` alone keeps accumulating until a `\n` that a hostile host need never
/// send, and one byte per socket timeout would hold the thread for days.
fn header_line<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    left: usize,
    deadline: Instant,
) -> std::io::Result<usize> {
    let mut bytes = Vec::new();
    while bytes.len() <= left {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let room = left + 1 - bytes.len();
        let end = available.iter().position(|b| *b == b'\n').map(|at| at + 1);
        let take = end.unwrap_or(available.len()).min(room);
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if end.is_some_and(|e| e <= room) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
    }
    line.push_str(&String::from_utf8_lossy(&bytes));
    Ok(bytes.len())
}

/// The first byte position of a `Content-Range: bytes <first>-<last>/<size>` value.
fn content_range_start(value: &str) -> Option<u64> {
    value
        .strip_prefix("bytes ")?
        .split_once('-')?
        .0
        .trim()
        .parse()
        .ok()
}

/// Sends `GET path` with `Range: bytes=<start>-` (when `start > 0`) and returns the reply.
fn request(url: &HttpUrl, path: &str, start: u64) -> Result<Reply, String> {
    // Last line of defence for the request line: no caller may smuggle a header or a second request.
    if path.contains(|c: char| c.is_ascii_control() || c == ' ') {
        return Err(format!("bad request path {path:?}"));
    }
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("{}: {e}", url.host))?
        .next()
        .ok_or_else(|| format!("{} has no address", url.host))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, IO_TIMEOUT).map_err(|e| format!("{}: {e}", url.host))?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let range = if start > 0 {
        format!("Range: bytes={start}-\r\n")
    } else {
        String::new()
    };
    let head = format!(
        "GET {path} HTTP/1.1\r\nHost: {}:{}\r\n{range}Connection: close\r\nUser-Agent: regia-content\r\n\r\n",
        url.host_for_url(),
        url.port
    );
    stream.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let too_long = || "bad HTTP response headers: longer than the header cap".to_string();
    let deadline = Instant::now() + HEADER_DEADLINE;
    if header_line(&mut reader, &mut line, MAX_HEADER_BYTES, deadline).map_err(|e| e.to_string())?
        > MAX_HEADER_BYTES
    {
        return Err(too_long());
    }
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("bad HTTP status line {line:?}"))?;
    let mut length = None;
    let mut range_start = None;
    let mut header_bytes = line.len();
    loop {
        line.clear();
        let left = MAX_HEADER_BYTES.saturating_sub(header_bytes);
        let n = header_line(&mut reader, &mut line, left, deadline).map_err(|e| e.to_string())?;
        header_bytes += n;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(too_long());
        }
        if n == 0 {
            return Err("bad HTTP response headers".into());
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            if name == "content-length" {
                length = value.trim().parse::<u64>().ok();
            } else if name == "content-range" {
                range_start = content_range_start(value.trim());
            } else if name == "transfer-encoding" && !value.trim().eq_ignore_ascii_case("identity") {
                return Err(
                    "chunked transfer encoding is not supported; serve files with Content-Length".into(),
                );
            }
        }
    }
    Ok(Reply {
        status,
        body: Box::new(reader),
        length,
        range_start,
    })
}

/// Largest manifest or `latest.ron` accepted from a host.
const MAX_MANIFEST_BYTES: u64 = 16 << 20;

/// GETs a small text file (`latest.ron`, a manifest).
fn get_text(client: &HttpClient, url: &HttpUrl, path: &str) -> Result<String, String> {
    let reply = client.get(url, path, 0)?;
    if reply.status != 200 {
        return Err(format!("HTTP {} for {path}", reply.status));
    }
    if reply.length.is_some_and(|l| l > MAX_MANIFEST_BYTES) {
        return Err(format!("{path} is larger than {MAX_MANIFEST_BYTES} bytes"));
    }
    let mut text = String::new();
    reply
        .body
        .take(MAX_MANIFEST_BYTES)
        .read_to_string(&mut text)
        .map_err(|e| format!("{path}: {e}"))?;
    Ok(text)
}

/// Fetches the content manifest a host offers (spec 40 § 2.6): `<base_url>/latest.ron` names the
/// newest build id (one line, optionally quoted), then `<base_url>/<build_id>/content-manifest.ron`
/// is read. The manifest must be for `running`'s platform and a build `running` accepts (its own
/// build id or one in its `compatible_with`).
///
/// # Errors
/// [`super::ContentError::SourceUnreachable`] when the host does not answer,
/// [`super::ContentError::ChunkBaseMismatch`] for another platform or an unaccepted build, and the
/// manifest's own parse errors.
pub fn fetch_manifest(
    base_url: &str,
    running: &super::manifest::ContentManifest,
) -> Result<super::manifest::ContentManifest, super::ContentError> {
    fetch_manifest_with(&HttpClient::new(), base_url, running)
}

/// [`fetch_manifest`] through a caller's [`HttpClient`].
///
/// # Errors
/// As [`fetch_manifest`]; an `https://` URL without the `content-http` feature, and a plain
/// `http://` URL to another machine without the developer override, are
/// [`super::ContentError::SourceNotSupported`].
pub fn fetch_manifest_with(
    client: &HttpClient,
    base_url: &str,
    running: &super::manifest::ContentManifest,
) -> Result<super::manifest::ContentManifest, super::ContentError> {
    fetch_manifest_trusted(client, base_url, running, None).map(|fetched| fetched.manifest)
}

/// A manifest a host offered, with the outcome of its signature check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedManifest {
    /// The manifest (checked against the running game's platform and accepted builds).
    pub manifest: super::manifest::ContentManifest,
    /// What the signature check found ([`super::SignatureCheck::NotConfigured`] without `trust`).
    pub signature: super::SignatureCheck,
}

/// [`fetch_manifest_with`] with signature checks (ADR-2710): with `trust`, the detached
/// `<base_url>/<build_id>/content-manifest.ron.sig` is fetched and checked against the manifest's
/// exact bytes **before** the manifest is parsed. Without `trust` this is
/// [`fetch_manifest_with`].
///
/// # Errors
/// As [`fetch_manifest_with`], plus [`super::ContentError::SignatureInvalid`] and
/// [`super::ContentError::SignatureMissing`] ([`super::check_manifest_signature`]).
pub fn fetch_manifest_trusted(
    client: &HttpClient,
    base_url: &str,
    running: &super::manifest::ContentManifest,
    trust: Option<&super::ContentTrust>,
) -> Result<FetchedManifest, super::ContentError> {
    use super::ContentError;
    let _span = tracing::info_span!(target: "regia_asset", "content_fetch_manifest").entered();
    let url = HttpUrl::parse(base_url).map_err(ContentError::SourceNotSupported)?;
    client.check(&url).map_err(ContentError::SourceNotSupported)?;
    let latest = get_text(client, &url, &format!("{}/latest.ron", url.path))
        .map_err(ContentError::SourceUnreachable)?;
    let build_id = latest.trim().trim_matches('"').to_owned();
    // Control characters too: the id goes into a request line, and CR/LF would split it.
    if super::manifest::validate_build_id(&build_id).is_err() {
        // Never the reply itself (ADR-5869): `content.check_host` answers a read-only agent
        // client, and the base URL may name any loopback service, so echoing the body turned the
        // method into a reader of local HTTP endpoints.
        return Err(ContentError::Format(format!(
            "latest.ron names no usable build id ({} bytes, {} lines)",
            latest.len(),
            latest.lines().count()
        )));
    }
    // A failed request names its path, which holds the id the host's reply gave: keep that reply
    // out of the error the same way (ADR-5869).
    let scrub =
        |e: String| ContentError::SourceUnreachable(e.replace(&build_id, "<build id from latest.ron>"));
    let text = get_text(
        client,
        &url,
        &format!(
            "{}/{build_id}/{}",
            url.path,
            super::manifest::CONTENT_MANIFEST_FILE
        ),
    )
    .map_err(scrub)?;
    // The signature is checked over the bytes as served, before anything parses them.
    let signature = match trust {
        None => super::SignatureCheck::NotConfigured,
        Some(trust) => {
            let sig = get_optional_bytes(
                client,
                &url,
                &format!("{}/{build_id}/{}", url.path, super::CONTENT_SIGNATURE_FILE),
                super::MAX_SIGNATURE_BYTES as u64,
            )
            .map_err(scrub)?;
            super::check_manifest_signature(Some(trust), text.as_bytes(), sig.as_deref())?
        }
    };
    let manifest = super::manifest::ContentManifest::from_ron_str(&text)?;
    if manifest.platform != running.platform || !running.accepts_base(&manifest.build_id) {
        return Err(ContentError::ChunkBaseMismatch {
            chunk: super::manifest::BASE_CHUNK.to_owned(),
            expected: format!("{} ({})", manifest.build_id, manifest.platform),
            running: format!("{} ({})", running.build_id, running.platform),
        });
    }
    Ok(FetchedManifest { manifest, signature })
}

/// GETs a small file that may be absent: `Ok(None)` for 404 and 410, at most `cap` bytes (a
/// longer body is an error, never cut short and checked).
fn get_optional_bytes(
    client: &HttpClient,
    url: &HttpUrl,
    path: &str,
    cap: u64,
) -> Result<Option<Vec<u8>>, String> {
    let reply = client.get(url, path, 0)?;
    match reply.status {
        200 => {}
        404 | 410 => return Ok(None),
        status => return Err(format!("HTTP {status} for {path}")),
    }
    if reply.length.is_some_and(|l| l > cap) {
        return Err(format!("{path} is larger than {cap} bytes"));
    }
    let mut bytes = Vec::new();
    reply
        .body
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{path}: {e}"))?;
    if bytes.len() as u64 > cap {
        return Err(format!("{path} is larger than {cap} bytes"));
    }
    Ok(Some(bytes))
}

/// Bytes served and requests answered by a [`serve_dir`] server.
#[derive(Debug, Default)]
pub struct ServeStats {
    /// Body bytes sent.
    pub bytes: AtomicU64,
    /// Requests answered (any status).
    pub requests: AtomicU64,
}

/// Serves `root` on `listener` until the process exits (or forever on a thread): one thread per
/// connection, `GET` and `HEAD`, `Range` support, `404` for anything missing or outside `root`.
pub fn serve_dir(listener: TcpListener, root: PathBuf, stats: Arc<ServeStats>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let root = root.clone();
        let stats = Arc::clone(&stats);
        let _detached = std::thread::Builder::new()
            .name("content-serve".into())
            .spawn(move || {
                if let Err(e) = answer(stream, &root, &stats) {
                    tracing::debug!(target: "regia_asset", "content serve: {e}");
                }
            });
    }
}

/// Starts [`serve_dir`] on `127.0.0.1:<OS-assigned port>` in a background thread and returns the
/// `http://` base URL and the stats (tests).
///
/// # Errors
/// The port cannot be bound.
pub fn serve_dir_loopback(root: PathBuf) -> std::io::Result<(String, Arc<ServeStats>)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let url = format!("http://{}", listener.local_addr()?);
    let stats = Arc::new(ServeStats::default());
    let thread_stats = Arc::clone(&stats);
    std::thread::Builder::new()
        .name("content-serve-loopback".into())
        .spawn(move || serve_dir(listener, root, thread_stats))?;
    Ok((url, stats))
}

fn answer(stream: TcpStream, root: &Path, stats: &ServeStats) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    serve_connection(stream, root, stats)
}

/// Answers one request on an already accepted connection (plain TCP, or a TLS stream a test or
/// tool wraps around one) the way [`serve_dir`] does: `GET`/`HEAD`, `Range`, `404` outside `root`.
///
/// # Errors
/// The connection fails.
pub fn serve_connection<S: Read + Write>(stream: S, root: &Path, stats: &ServeStats) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let deadline = Instant::now() + HEADER_DEADLINE;
    header_line(&mut reader, &mut line, MAX_HEADER_BYTES, deadline)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("").to_owned();
    let mut range: Option<(u64, Option<u64>)> = None;
    let mut header_bytes = line.len();
    // The blank line ends the headers. A peer that closes its write half first, or whose header
    // block outgrows the cap, gets `400`: answering from the request line alone would serve the
    // file without ever seeing headers such as `Range` (audit asset1).
    let mut incomplete_headers = false;
    loop {
        let mut header = String::new();
        let left = MAX_HEADER_BYTES.saturating_sub(header_bytes);
        let n = header_line(&mut reader, &mut header, left, deadline)?;
        header_bytes += n;
        if header_bytes > MAX_HEADER_BYTES {
            incomplete_headers = true;
            break;
        }
        if n == 0 {
            incomplete_headers = true;
            break;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("range")
        {
            range = parse_range(value.trim());
        }
    }
    stats.requests.fetch_add(1, Ordering::Relaxed);
    // A GET carries no body, so nothing buffered is lost by writing to the inner stream.
    let out = reader.get_mut();
    let respond = |out: &mut S, status: &str| {
        out.write_all(
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
        )
    };
    if incomplete_headers {
        return respond(out, "400 Bad Request");
    }
    if method != "GET" && method != "HEAD" {
        return respond(out, "405 Method Not Allowed");
    }
    let Some(path) = resolve(root, &target) else {
        return respond(out, "404 Not Found");
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return respond(out, "404 Not Found");
    };
    let len = file.metadata()?.len();
    let (status, start, end) = match range {
        Some((start, end)) if start < len => {
            let end = end.map_or(len - 1, |e| e.min(len - 1));
            ("206 Partial Content", start, end)
        }
        Some(_) => return respond(out, "416 Range Not Satisfiable"),
        None => ("200 OK", 0, len.saturating_sub(1)),
    };
    let body_len = if len == 0 { 0 } else { end - start + 1 };
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {body_len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n"
    );
    if status.starts_with("206") {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    if method == "HEAD" {
        return Ok(());
    }
    file.seek(SeekFrom::Start(start))?;
    let sent = std::io::copy(&mut file.take(body_len), &mut *out)?;
    stats.bytes.fetch_add(sent, Ordering::Relaxed);
    out.flush()
}

/// `bytes=N-` or `bytes=N-M`.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = match end.trim() {
        "" => None,
        e => Some(e.parse().ok()?),
    };
    // RFC 9110: an invalid range is ignored. `end - start + 1` below it would underflow.
    if end.is_some_and(|e: u64| e < start) {
        return None;
    }
    Some((start, end))
}

/// The file under `root` a request target names; `None` for anything that could leave `root`
/// (`..`, absolute components) or a directory.
fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next()?.trim_start_matches('/');
    if path.is_empty() || path.contains('\\') || path.contains('\0') {
        return None;
    }
    let rel = Path::new(path);
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let full = root.join(rel);
    if !full.is_file() {
        return None;
    }
    // A link under the root must not serve a file outside it (audit v0.2.12: `is_file` and the
    // open follow links).
    let real_root = root.canonicalize().ok()?;
    full.canonicalize()
        .ok()
        .filter(|real| real.starts_with(&real_root))
        .map(|_| full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_both_schemes() {
        let u = HttpUrl::parse("http://127.0.0.1:8080/game/").expect("url");
        assert_eq!(
            (u.https, u.host.as_str(), u.port, u.path.as_str()),
            (false, "127.0.0.1", 8080, "/game")
        );
        assert_eq!(HttpUrl::parse("http://cdn.example.com").expect("url").port, 80);
        let tls = HttpUrl::parse("https://cdn.example.com/g").expect("https url");
        assert_eq!((tls.https, tls.port, tls.path.as_str()), (true, 443, "/g"));
        assert_eq!(tls.origin(), "https://cdn.example.com:443");
        assert!(HttpUrl::parse("ftp://x").is_err());
        assert!(HttpUrl::parse("http://:80/").is_err());
        assert!(HttpUrl::parse("https://h:99999/").is_err());
    }

    /// Audit v0.2.12 (content L6): the port was split at the last `:` of an IPv6 address.
    #[test]
    fn ipv6_urls_parse_and_connect_on_loopback() {
        let u = HttpUrl::parse("http://[::1]:8787/game").expect("v6 url");
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("::1", 8787, "/game"));
        assert!(u.is_loopback());
        assert_eq!(u.origin(), "http://[::1]:8787");
        let d = HttpUrl::parse("https://[2001:db8::1]/g").expect("v6 default port");
        assert_eq!((d.host.as_str(), d.port), ("2001:db8::1", 443));
        for bad in [
            "http://[::1/x",
            "http://[nothex]:80/",
            "http://[::1]x/",
            "http://user@h/x",
        ] {
            assert!(HttpUrl::parse(bad).is_err(), "{bad}");
        }
        // A real request over IPv6 loopback, where the host has it.
        let Ok(listener) = TcpListener::bind(("::1", 0)) else {
            return;
        };
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut host = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if line.to_ascii_lowercase().starts_with("host:") {
                    host = line.trim().to_owned();
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            host
        });
        let url = HttpUrl::parse(&format!("http://[::1]:{port}")).expect("url");
        let mut reply = request(&url, "/x", 0).expect("request over ::1");
        assert_eq!(reply.status, 200);
        let mut body = String::new();
        reply.body.read_to_string(&mut body).expect("body");
        assert_eq!(body, "ok");
        assert_eq!(server.join().expect("server"), format!("Host: [::1]:{port}"));
    }

    #[test]
    fn https_sources_follow_the_feature() {
        let made = HttpSource::new("https://cdn.example.com/game", "1-1-x");
        assert_eq!(made.is_ok(), HttpClient::supports_https());
        if let Err(why) = made {
            assert!(why.contains("content-http"), "{why}");
        }
        let plain = HttpClient::new().allow_plain_http(true);
        assert!(HttpSource::with_client("http://cdn.example.com/game", "1-1-x", plain).is_ok());
        assert!(HttpSource::new("http://127.0.0.1:8787/game", "1-1-x").is_ok());
    }

    /// Audit v0.2.12 (security): plain `http://` content from another machine has no authenticity
    /// root (the chunk hashes come from the same unauthenticated manifest), so it is refused
    /// unless a developer opts in. Loopback stays open for tests and `regia content serve`.
    #[test]
    fn plain_http_to_another_machine_needs_the_developer_override() {
        let strict = HttpClient::default();
        for url in [
            "http://cdn.example.com/game",
            "http://192.168.1.20:8787/g",
            "http://10.0.0.1",
        ] {
            let Err(why) = HttpSource::with_client(url, "1-1-x", strict.clone()) else {
                panic!("{url} must be refused without the override");
            };
            assert!(why.contains(CONTENT_ALLOW_HTTP_ENV), "{why}");
        }
        for url in [
            "http://127.0.0.1:8787/g",
            "http://localhost/g",
            "http://127.1.2.3/g",
        ] {
            assert!(
                HttpSource::with_client(url, "1-1-x", strict.clone()).is_ok(),
                "{url} is this machine"
            );
        }
        let dev = strict.allow_plain_http(true);
        assert!(HttpSource::with_client("http://cdn.example.com/game", "1-1-x", dev).is_ok());
    }

    /// Audit v0.2.12 (security): a build id from a local manifest went into the request line
    /// unchecked, so CR/LF split it and `/` or `..` named another CDN path.
    #[test]
    fn a_source_refuses_a_build_id_that_would_break_the_request_line() {
        for bad in [
            "1-0-x\r\nHost: evil\r\nX: ",
            "2026/11/01",
            "../up",
            "a b",
            "",
            "x\0y",
        ] {
            let made = HttpSource::new("http://127.0.0.1:8787/game", bad);
            assert!(made.is_err(), "{bad:?} must be refused");
        }
        assert!(HttpSource::new("http://127.0.0.1:8787/game", "1.2.0-7-0123456789ab").is_ok());
        let url = HttpUrl::parse("http://127.0.0.1:9").expect("url");
        let Err(why) = request(&url, "/a\r\nX: y", 0) else {
            panic!("a path with CR/LF must not be sent");
        };
        assert!(why.contains("bad request path"), "{why}");
    }

    #[test]
    fn a_header_line_stops_at_the_cap_without_a_newline() {
        let mut stream = std::io::Cursor::new(vec![b'a'; 1 << 20]);
        let mut line = String::new();
        let later = Instant::now() + HEADER_DEADLINE;
        let n = header_line(&mut stream, &mut line, MAX_HEADER_BYTES, later).expect("read");
        assert_eq!(n, MAX_HEADER_BYTES + 1);
        assert_eq!(line.len(), MAX_HEADER_BYTES + 1);
        let mut short = std::io::Cursor::new(b"HTTP/1.1 200 OK\r\nNext: x\r\n".to_vec());
        line.clear();
        assert_eq!(
            header_line(&mut short, &mut line, MAX_HEADER_BYTES, later).expect("read"),
            17
        );
        assert_eq!(line, "HTTP/1.1 200 OK\r\n");
    }

    /// Review finding: one byte per socket timeout restarts the timeout, so the header needs its own
    /// deadline. A deadline already passed stands in for the wait.
    #[test]
    fn a_header_line_past_its_deadline_times_out() {
        let mut stream = std::io::Cursor::new(vec![b'a'; 100]);
        let mut line = String::new();
        let err = header_line(&mut stream, &mut line, MAX_HEADER_BYTES, Instant::now()).expect_err("late");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        // A complete line in hand is an answer, however late.
        let mut stream = std::io::Cursor::new(b"ok\n".to_vec());
        assert_eq!(
            header_line(&mut stream, &mut line, MAX_HEADER_BYTES, Instant::now()).expect("line"),
            3
        );
    }

    /// A host that answers one endless header line, and one that sends it with the status line.
    #[test]
    fn an_endless_header_line_fails_the_request() {
        for reply in [b"HTTP/1.1 200 OK\r\nX: ".to_vec(), Vec::new()] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept");
                let _ = stream.write_all(&reply);
                // Far past the cap, and it never sends a newline: finite so a failure is a wrong
                // error, not a hung test.
                for _ in 0..64 {
                    if stream.write_all(&[b'a'; 4096]).is_err() {
                        break;
                    }
                }
            });
            let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}")).expect("url");
            let Err(why) = request(&url, "/x", 0) else {
                panic!("a header past the cap must fail the request");
            };
            assert!(why.contains("header cap"), "{why}");
            let _ = server.join();
        }
    }

    /// Audit (asset1): a peer that closed the write half before the blank line, or whose headers
    /// ran past the cap with `Range` after it, was answered from the request line alone (`200`
    /// and the whole file). Both get `400` now; only a complete header block is served.
    #[test]
    fn truncated_or_overcapped_headers_are_answered_400() {
        let dir = regia_test_utils::unique_temp_dir("http_truncated_headers");
        std::fs::write(dir.join("pack.bin"), b"0123456789").expect("file");
        let stats = ServeStats::default();
        let status_line = |raw: &[u8]| -> String {
            let at = raw
                .windows(8)
                .rposition(|w| w == b"HTTP/1.1")
                .expect("a written status line");
            String::from_utf8(raw[at..at + 15].to_vec()).expect("ascii status line")
        };
        // The peer closes the write half before the blank line.
        let mut stream = std::io::Cursor::new(b"GET /pack.bin HTTP/1.1\r\nHost: 127.0.0.1\r\n".to_vec());
        serve_connection(&mut stream, &dir, &stats).expect("serve");
        let line = status_line(stream.get_ref());
        assert!(line.starts_with("HTTP/1.1 400"), "{line}");
        // Headers past the cap, with the `Range` sitting after it.
        let mut over = b"GET /pack.bin HTTP/1.1\r\n".to_vec();
        for _ in 0..2000 {
            over.extend_from_slice(b"X-a: 0123456789abcdef\r\n");
        }
        over.extend_from_slice(b"Range: bytes=0-0\r\n\r\n");
        let mut stream = std::io::Cursor::new(over);
        serve_connection(&mut stream, &dir, &stats).expect("serve");
        let line = status_line(stream.get_ref());
        assert!(line.starts_with("HTTP/1.1 400"), "{line}");
        // A complete request is still served.
        let mut stream = std::io::Cursor::new(b"GET /pack.bin HTTP/1.1\r\nHost: x\r\n\r\n".to_vec());
        serve_connection(&mut stream, &dir, &stats).expect("serve");
        let line = status_line(stream.get_ref());
        assert!(line.starts_with("HTTP/1.1 200"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_content_range_names_its_first_byte() {
        assert_eq!(
            content_range_start("bytes 5000000-5999999/6000000"),
            Some(5_000_000)
        );
        assert_eq!(content_range_start("bytes 0-9/*"), Some(0));
        assert_eq!(content_range_start("bytes */100"), None);
        assert_eq!(content_range_start("items 0-9/10"), None);
    }

    /// A host that answers 206 from the wrong offset once, then whatever the client asks.
    fn wrong_range_host(body: Vec<u8>, ranges_asked: Arc<AtomicU64>) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut asked_range = false;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    asked_range |= line.to_ascii_lowercase().starts_with("range:");
                }
                let head = if asked_range {
                    ranges_asked.fetch_add(1, Ordering::Relaxed);
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {n}\r\nContent-Range: bytes 0-{last}/{n}\r\n\r\n",
                        n = body.len(),
                        last = body.len() - 1
                    )
                } else {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                };
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        url
    }

    /// contentdel L5: a 206 for other bytes was written at the requested offset, so the hash failed
    /// on every retry. The job asks again from zero and the file comes out whole.
    #[test]
    fn a_206_for_the_wrong_bytes_restarts_from_zero() {
        let dir = regia_test_utils::unique_temp_dir("content_wrong_range");
        let dest = dir.join("c.fpak.part");
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&dest, &body[..150_000]).expect("part");
        let asked = Arc::new(AtomicU64::new(0));
        let url = HttpUrl::parse(&wrong_range_host(body.clone(), Arc::clone(&asked))).expect("url");
        let job = Job {
            client: HttpClient::new(),
            url,
            path: "/c.fpak".to_string(),
            dest: dest.clone(),
            total: body.len() as u64,
            blocks: Vec::new(),
            shared: Arc::new(Shared::default()),
        };
        job.run().expect("the transfer restarts");
        assert_eq!(asked.load(Ordering::Relaxed), 1, "the range was asked for once");
        assert_eq!(std::fs::read(&dest).expect("read"), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A body longer than the manifest size must not leave extra bytes in the `.part` file.
    #[test]
    fn a_body_longer_than_the_manifest_size_is_cut_at_the_size() {
        let dir = regia_test_utils::unique_temp_dir("content_long_body");
        let dest = dir.join("c.fpak.part");
        let body: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let url = HttpUrl::parse(&wrong_range_host(body.clone(), Arc::new(AtomicU64::new(0)))).expect("url");
        let job = Job {
            client: HttpClient::new(),
            url,
            path: "/c.fpak".to_string(),
            dest: dest.clone(),
            total: 70_000,
            blocks: Vec::new(),
            shared: Arc::new(Shared::default()),
        };
        job.run().expect("the transfer completes");
        assert_eq!(std::fs::read(&dest).expect("read"), body[..70_000]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bracketed_ipv6_hosts_parse_and_round_trip() {
        let u = HttpUrl::parse("http://[::1]/chunks").expect("url");
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("::1", 80, "/chunks"));
        assert_eq!(u.origin(), "http://[::1]:80");
        let u = HttpUrl::parse("https://[fe80::1]:8443/a").expect("url");
        assert_eq!((u.host.as_str(), u.port), ("fe80::1", 8443));
        assert_eq!(u.origin(), "https://[fe80::1]:8443");
        assert!(HttpUrl::parse("http://[::1/x").is_err());
        assert!(HttpUrl::parse("http://[::1]x/x").is_err());
        assert!(HttpUrl::parse("http://[]/x").is_err());
        assert!(HttpUrl::parse("http://[::1]:99999/x").is_err());
        // The socket address lookup accepts the unbracketed form.
        assert!((u.host.as_str(), u.port).to_socket_addrs().is_ok());
    }

    /// contentdel L6: synthetic times, so the rule is pinned without a real minute.
    #[test]
    fn a_transfer_below_the_floor_for_a_whole_window_stalls() {
        let mut g = StallGuard::default();
        assert!(g.note(1_000, 1), "inside the first window");
        assert!(g.note(STALL_WINDOW_MS - 1, 1));
        assert!(!g.note(STALL_WINDOW_MS, 1), "three bytes in a window");
        let mut g = StallGuard::default();
        for t in 1..=10 {
            assert!(
                g.note(t * STALL_WINDOW_MS, STALL_MIN_BYTES),
                "a slow host that moves is kept"
            );
        }
    }

    /// contentdel L7: a second job on one `.part` waits for the first, and gives up when cancelled.
    #[test]
    fn a_part_file_has_one_writer_at_a_time() {
        let dir = regia_test_utils::unique_temp_dir("content_lease");
        let path = dir.join("c.fpak.part");
        let live = AtomicBool::new(false);
        let cancelled = AtomicBool::new(true);
        let first = PartLease::acquire(&path, &live).expect("free");
        assert!(
            PartLease::acquire(&path, &cancelled).is_none(),
            "held: a cancelled job gives up"
        );
        let other = PartLease::acquire(&dir.join("other.part"), &live).expect("another file is free");
        drop(first);
        assert!(PartLease::acquire(&path, &cancelled).is_some(), "free again");
        drop(other);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ranges_and_paths() {
        assert_eq!(parse_range("bytes=10-"), Some((10, None)));
        assert_eq!(parse_range("bytes=10-19"), Some((10, Some(19))));
        assert_eq!(parse_range("items=1-2"), None);
        // `regia content serve` underflowed on this: a debug panic, a bogus length in release.
        assert_eq!(parse_range("bytes=10-5"), None);
        assert_eq!(parse_range("bytes=10-10"), Some((10, Some(10))));
        let root = std::env::temp_dir();
        assert!(resolve(&root, "/../etc/passwd").is_none());
        assert!(resolve(&root, "/a/../../b").is_none());
        assert!(resolve(&root, "/").is_none());
    }

    /// Audit v0.2.12 (content L7): `regia content serve` followed a link out of its root.
    #[cfg(unix)]
    #[test]
    fn serve_does_not_follow_a_link_out_of_its_root() {
        let dir = regia_test_utils::unique_temp_dir("content_serve_link");
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("sub")).expect("root");
        std::fs::write(dir.join("secret.txt"), b"secret").expect("secret");
        std::fs::write(root.join("sub/ok.fpak"), b"ok").expect("ok");
        std::os::unix::fs::symlink(dir.join("secret.txt"), root.join("leak.txt")).expect("link");
        std::os::unix::fs::symlink(root.join("sub/ok.fpak"), root.join("inside.fpak")).expect("link");
        assert!(resolve(&root, "/leak.txt").is_none(), "outside the root");
        assert!(resolve(&root, "/sub/ok.fpak").is_some());
        assert!(
            resolve(&root, "/inside.fpak").is_some(),
            "a link that stays inside is served"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit concurrency-1103 asset H1: a parked HTTP job may still sit in a socket read holding
    /// its `.part` lease when the chunk restarts from a directory source. The directory transfer
    /// must wait for the lease instead of truncating and writing the same inode under it, and
    /// the two spellings of one directory share one lease (L5).
    #[test]
    fn a_dir_transfer_waits_for_the_part_lease_of_a_parked_http_job() {
        let dir = regia_test_utils::unique_temp_dir("content_lease_dir");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).expect("root");
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(root.join("c.fpak"), &body).expect("source");
        let dest = dir.join("c.fpak.part");
        // The lease the HTTP job takes, spelled through a `.` component.
        let held =
            PartLease::acquire(&dir.join(".").join("c.fpak.part"), &AtomicBool::new(false)).expect("free");
        let chunk = ChunkEntry {
            name: "c".into(),
            file: "c.fpak".into(),
            size: body.len() as u64,
            hash: String::new(),
            delivery: super::super::manifest::Delivery::OnDemand,
            requires: Vec::new(),
            overrides: false,
            mount_order: 1,
            assets: 0,
            blocks: Vec::new(),
        };
        let source = super::super::source::DirSource::new(&root);
        let mut transfer = source.start(&chunk, &dest);
        let step = transfer.step(u64::MAX);
        assert!(
            !matches!(step, TransferStep::Done | TransferStep::Failed(_)),
            "the directory transfer wrote the .part while the HTTP job held its lease: {step:?}"
        );
        assert_eq!(
            std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
            0,
            "no byte is written under another writer's lease"
        );
        drop(held);
        let mut last = transfer.step(u64::MAX);
        for _ in 0..4 {
            if matches!(last, TransferStep::Done | TransferStep::Failed(_)) {
                break;
            }
            last = transfer.step(u64::MAX);
        }
        assert!(matches!(last, TransferStep::Done), "{last:?}");
        assert_eq!(std::fs::read(&dest).expect("part"), body);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
