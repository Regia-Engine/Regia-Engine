// SPDX-License-Identifier: MPL-2.0
//! The upload transport: a synchronous HTTP/1.1 `POST`.
//!
//! `http://` endpoints use a dependency-free `std::net::TcpStream` transport (suitable for local
//! dev servers and the offline tests). `https://` endpoints need the crate's `tls` feature, which
//! brings in `ureq` + `rustls` (`ring` provider — no native-tls/openssl); without that feature an
//! `https://` URL is refused with [`TransportError::TlsUnavailable`], so the reporter never silently
//! downgrades to plaintext. Production crash-report endpoints must be `https://` (specs/21 § 5,
//! docs/03 § 7).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Transport failures.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The endpoint is `https://` but no TLS backend is compiled in (the `tls` feature is off).
    #[error("https:// uploads need the `tls` feature (ureq + rustls): {url}")]
    TlsUnavailable {
        /// The rejected URL.
        url: String,
    },
    /// The URL could not be understood.
    #[error("invalid upload URL {url:?}: {reason}")]
    InvalidUrl {
        /// The rejected URL.
        url: String,
        /// Why.
        reason: String,
    },
    /// A header value contained CR/LF or a non-token (control/non-ASCII) character.
    #[error("invalid HTTP header {header}: {value:?} contains CR/LF or a non-token character")]
    InvalidHeader {
        /// The header name (`Content-Type`, [`REPORT_NAME_HEADER`], …).
        header: String,
        /// The rejected value.
        value: String,
    },
    /// The reporter has no endpoint, or consent is off. Nothing was sent.
    ///
    /// A remote `http://` endpoint with consent and `allow_cleartext` left off is
    /// [`Self::RemoteCleartext`], not this variant.
    #[error("uploads are disabled (no endpoint or no consent)")]
    Disabled,
    /// The endpoint is a remote `http://` URL and `allow_cleartext` is off. Nothing was sent.
    ///
    /// "Remote" is a host-string check ([`is_remote_cleartext`]): the host is not `localhost`
    /// and not a literal loopback IP. Names are not resolved.
    #[error("uploads are disabled (remote http without allow_cleartext)")]
    RemoteCleartext,
    /// The report exceeds the configured size cap and was not sent.
    #[error("report is {size} bytes, over the {max_bytes}-byte cap")]
    TooLarge {
        /// The report size in bytes.
        size: u64,
        /// The configured cap.
        max_bytes: u64,
    },
    /// The host could not be resolved or connected.
    #[error("cannot connect to {host}: {source}")]
    Connect {
        /// The host:port that was dialled.
        host: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// An IO error while sending or reading the response.
    #[error("IO error talking to {host}: {source}")]
    Io {
        /// The host:port that was dialled.
        host: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The TLS-backed client (ureq) failed to send the request.
    #[error("request failed for {url}: {message}")]
    Request {
        /// The URL that was requested.
        url: String,
        /// Why (connection refused, TLS handshake, timeout, …).
        message: String,
    },
}

/// Timeouts for one upload attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportTimeouts {
    /// TCP connect timeout.
    pub connect: Duration,
    /// Read timeout (response headers + body drain).
    pub read: Duration,
    /// Write timeout (request).
    pub write: Duration,
}

impl Default for TransportTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            read: Duration::from_secs(30),
            write: Duration::from_secs(30),
        }
    }
}

/// The longest timeout handed to ureq (one year): it adds timeouts to `Instant::now()`
/// unchecked, so `Duration::MAX` (no limit) would panic there (audit-concurrency-1103 crash L1).
#[cfg(feature = "tls")]
const MAX_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// How many write timeouts the whole request (headers + body) may take. The per-call socket write
/// timeout alone does not bound a peer that accepts a byte at a time, so the send also has an
/// overall deadline of this many multiples of [`TransportTimeouts::write`].
const SEND_DEADLINE_FACTOR: u32 = 4;

/// The largest chunk handed to one `write` call, so the deadline is checked often.
const SEND_CHUNK: usize = 16 * 1024;

/// Writes all of `buf`, in chunks, failing with `TimedOut` as soon as `expired` reports true
/// between two writes. A slow peer that makes tiny progress on every call therefore cannot hold
/// the socket past the deadline.
fn write_all_bounded<W: Write>(
    writer: &mut W,
    mut buf: &[u8],
    expired: &mut dyn FnMut() -> bool,
) -> std::io::Result<()> {
    while !buf.is_empty() {
        if expired() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upload deadline exceeded",
            ));
        }
        let take = buf.len().min(SEND_CHUNK);
        match writer.write(&buf[..take]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write the request body",
                ));
            }
            Ok(n) => buf = &buf[n..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Which scheme a URL carries (before any host/path parsing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    /// `http://`.
    Http,
    /// `https://`.
    Https,
}

/// Whether a POST to `url` is treated as cleartext to a remote host.
///
/// True only for `http://` whose host string is neither `localhost` (ASCII case-insensitive) nor
/// a literal loopback IP (`127.0.0.1`, `::1`, and any other address `IpAddr::is_loopback` accepts).
/// The name is not resolved and the connected peer is not inspected, so a LAN address of this
/// machine counts as remote, and `localhost` counts as local even when a resolver would not
/// return a loopback address. An `https://` URL is not cleartext, and a URL that does not parse
/// is not either (it fails before it sends).
#[must_use]
pub fn is_remote_cleartext(url: &str) -> bool {
    if !matches!(scheme_of(url), Ok(Scheme::Http)) {
        return false;
    }
    parse_endpoint(url).is_ok_and(|endpoint| {
        let host = endpoint.host.as_str();
        !host.eq_ignore_ascii_case("localhost")
            && !host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
    })
}

/// `scheme://host[:port]` of `url`: no userinfo, path or query, so an error message (which the
/// reporter logs, and the log ring is copied into the next crash report) never carries a secret.
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<invalid url>".to_owned();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    format!("{scheme}://{host}")
}

/// Refuses `scheme://user:pass@host/...`: ureq would send the userinfo as Basic auth, and the
/// transport never carries credentials. The error shows the redacted URL.
fn reject_userinfo(url: &str) -> Result<(), TransportError> {
    let authority = url
        .split_once("://")
        .map_or("", |(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""));
    if authority.contains('@') {
        return Err(TransportError::InvalidUrl {
            url: redact_url(url),
            reason: "credentials in the URL are not supported".into(),
        });
    }
    Ok(())
}

fn scheme_of(url: &str) -> Result<Scheme, TransportError> {
    let (scheme, _) = url.split_once("://").ok_or_else(|| TransportError::InvalidUrl {
        url: url.to_owned(),
        reason: "missing `://` scheme".into(),
    })?;
    if scheme.eq_ignore_ascii_case("http") {
        Ok(Scheme::Http)
    } else if scheme.eq_ignore_ascii_case("https") {
        Ok(Scheme::Https)
    } else {
        Err(TransportError::InvalidUrl {
            url: url.to_owned(),
            reason: format!("unsupported scheme {scheme:?}"),
        })
    }
}

/// Longest status line accepted from a server; a peer that never sends `\n` is cut off here.
const MAX_STATUS_LINE: u64 = 8 * 1024;
/// Most response-header bytes read after the status line.
const MAX_DRAIN_BYTES: u64 = 64 * 1024;
/// Longest request target accepted.
const MAX_TARGET_LEN: usize = 2048;

/// Reads one response line, failing when `MAX_STATUS_LINE` bytes arrive without a newline.
fn read_status_line<R: BufRead>(reader: &mut R) -> std::io::Result<String> {
    read_line_capped(reader, MAX_STATUS_LINE)
}

/// Reads one `\n`-terminated line of at most `cap` bytes. A longer line (or one the peer never
/// terminates) fails with `InvalidData` instead of growing a buffer without bound.
fn read_line_capped<R: BufRead>(reader: &mut R, cap: u64) -> std::io::Result<String> {
    let mut line = Vec::new();
    reader
        .by_ref()
        .take(cap.saturating_add(1))
        .read_until(b'\n', &mut line)?;
    if line.len() as u64 > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response line too long",
        ));
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// A parsed `http://` endpoint.
struct Endpoint {
    host: String,
    port: u16,
    path: String,
}

/// Splits `http://host[:port][/path]` into host, port and path. Only `http://` reaches here
/// (`https://` is routed to the TLS backend first).
fn parse_endpoint(url: &str) -> Result<Endpoint, TransportError> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| TransportError::InvalidUrl {
        url: url.to_owned(),
        reason: "missing `://` scheme".into(),
    })?;
    if !scheme.eq_ignore_ascii_case("http") {
        return Err(TransportError::InvalidUrl {
            url: url.to_owned(),
            reason: format!("unsupported scheme {scheme:?}"),
        });
    }
    if rest.bytes().any(|b| !(0x21..=0x7E).contains(&b)) {
        return Err(TransportError::InvalidUrl {
            url: url.to_owned(),
            reason: "contains whitespace, control or non-ASCII characters".into(),
        });
    }
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_owned()),
    };
    let invalid_port = || TransportError::InvalidUrl {
        url: url.to_owned(),
        reason: "invalid port".into(),
    };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        // IPv6 literal: `[::1]` or `[::1]:8080`. The host keeps no brackets (the resolver wants
        // the bare address); the `Host` header re-adds them.
        let (host, after) = bracketed
            .split_once(']')
            .ok_or_else(|| TransportError::InvalidUrl {
                url: url.to_owned(),
                reason: "unterminated `[` in IPv6 host".into(),
            })?;
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse::<u16>().map_err(|_| invalid_port())?,
            None if after.is_empty() => 80,
            None => return Err(invalid_port()),
        };
        (host.to_owned(), port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_owned(), port.parse::<u16>().map_err(|_| invalid_port())?),
            None => (authority.to_owned(), 80),
        }
    };
    if host.is_empty() {
        return Err(TransportError::InvalidUrl {
            url: url.to_owned(),
            reason: "empty host".into(),
        });
    }
    // The host and path are written raw into the request line and `Host:` header, so refuse
    // controls (CR/LF/NUL), spaces and non-ASCII, and userinfo, before anything is sent.
    let unsafe_byte = |b: u8| !(0x21..=0x7E).contains(&b);
    if host.bytes().any(|b| unsafe_byte(b) || b == b'@') || path.bytes().any(unsafe_byte) {
        return Err(TransportError::InvalidUrl {
            url: url.to_owned(),
            reason: "host or path contains a control, space, non-ASCII or `@` character".into(),
        });
    }
    if path.len() > MAX_TARGET_LEN {
        return Err(TransportError::InvalidUrl {
            url: url.to_owned(),
            reason: "request target too long".into(),
        });
    }
    Ok(Endpoint { host, port, path })
}

/// Rejects a header value that could inject CR/LF or otherwise break the request framing: only
/// visible ASCII (space through `~`, `0x20..=0x7E`) is allowed. CR, LF, other control characters
/// and non-ASCII bytes are non-token characters here.
fn validate_header_value(header: &str, value: &str) -> Result<(), TransportError> {
    if value.bytes().any(|b| !(0x20..=0x7E).contains(&b)) {
        return Err(TransportError::InvalidHeader {
            header: header.to_owned(),
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// The header that carries a report part's name (`<file>;<start>-<end>`) so the receiving
/// service can attribute it (ADR-8670).
pub const REPORT_NAME_HEADER: &str = "X-Regia-Report-Name";
/// The name of [`REPORT_NAME_HEADER`] before ADR-8670. Every upload still sends it too, with the
/// same value, so a receiving service that matches only the old header keeps working; it is
/// dropped one release after v0.6.0 (`book/agents/report-outbox.md`).
pub const LEGACY_REPORT_NAME_HEADER: &str = "X-Ferrite-Report-Name"; // rename-keep: sent for one release (ADR-8670)

fn validate_headers(content_type: &str, name: Option<&str>) -> Result<(), TransportError> {
    validate_header_value("Content-Type", content_type)?;
    if let Some(name) = name {
        validate_header_value(REPORT_NAME_HEADER, name)?;
    }
    Ok(())
}

/// POSTs `body` to `url` and returns the HTTP status code. `name`, when given, is sent as the
/// [`REPORT_NAME_HEADER`] (and, for one release, the [`LEGACY_REPORT_NAME_HEADER`]) so the
/// receiving service can attribute the report.
///
/// `http://` uses the dependency-free transport; `https://` needs the `tls` feature (otherwise it
/// fails with [`TransportError::TlsUnavailable`]). Statuses are returned as-is (redirects are never followed) — callers treat any
/// 2xx as success. Both header values are validated against CR/LF injection first.
pub fn post(
    url: &str,
    body: &[u8],
    content_type: &str,
    name: Option<&str>,
    timeouts: TransportTimeouts,
) -> Result<u16, TransportError> {
    validate_headers(content_type, name)?;
    reject_userinfo(url)?;
    match scheme_of(url)? {
        Scheme::Https => {
            #[cfg(feature = "tls")]
            {
                if timeouts == TransportTimeouts::default() {
                    post_https(default_agent(), url, body, content_type, name, timeouts)
                } else {
                    post_https(&build_agent(timeouts), url, body, content_type, name, timeouts)
                }
            }
            #[cfg(not(feature = "tls"))]
            {
                Err(TransportError::TlsUnavailable { url: url.to_owned() })
            }
        }
        Scheme::Http => post_http(url, body, content_type, name, timeouts),
    }
}

/// Like [`post`] but through a caller-supplied [`ureq::Agent`], so a test can trust only a
/// self-signed certificate. Only available with the `tls` feature.
#[cfg(feature = "tls")]
pub fn post_with_agent(
    agent: &ureq::Agent,
    url: &str,
    body: &[u8],
    content_type: &str,
    name: Option<&str>,
    timeouts: TransportTimeouts,
) -> Result<u16, TransportError> {
    validate_headers(content_type, name)?;
    reject_userinfo(url)?;
    post_https(agent, url, body, content_type, name, timeouts)
}

#[cfg(feature = "tls")]
fn default_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| build_agent(TransportTimeouts::default()))
}

/// An agent honoring `timeouts`. It never follows redirects: a 301/302/303 turns a POST into a
/// body-less GET, so following one could report success for a request that carried no report.
/// Hosts that pass their own agent to [`post_with_agent`] should build it here: ureq has no
/// per-request redirect override, so a caller's agent keeps its own redirect policy.
#[cfg(feature = "tls")]
pub fn build_agent(timeouts: TransportTimeouts) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .tls_connector(crate::tls_guard::default_connector())
        // One response per stream: the response-head guard only filters the first head.
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .redirects(0)
        .timeout_connect(timeouts.connect.min(MAX_TIMEOUT))
        .timeout_read(timeouts.read.min(MAX_TIMEOUT))
        .timeout_write(timeouts.write.min(MAX_TIMEOUT))
        .build()
}

/// `https://` (or any scheme) through ureq + rustls. Returns the status code, mapping ureq's
/// status-carrying error back to a plain status so the reporter's retry/2xx logic is transport-agnostic.
#[cfg(feature = "tls")]
fn post_https(
    agent: &ureq::Agent,
    url: &str,
    body: &[u8],
    content_type: &str,
    name: Option<&str>,
    timeouts: TransportTimeouts,
) -> Result<u16, TransportError> {
    let mut request = agent.post(url).set("Content-Type", content_type);
    // The agent sets the per-phase timeouts; this overall deadline only bounds a slow trickle and
    // must not undercut them. (Redirects are disabled on agents built by `build_agent`.)
    request = request.timeout(
        timeouts
            .connect
            .saturating_add(timeouts.read)
            .saturating_add(timeouts.write)
            .min(MAX_TIMEOUT),
    );
    if let Some(name) = name {
        request = request
            .set(REPORT_NAME_HEADER, name)
            .set(LEGACY_REPORT_NAME_HEADER, name);
    }
    match request.send_bytes(body) {
        Ok(response) => Ok(response.status()),
        Err(ureq::Error::Status(code, _)) => Ok(code),
        Err(ureq::Error::Transport(transport)) => Err(match transport.kind() {
            ureq::ErrorKind::InvalidUrl
            | ureq::ErrorKind::UnknownScheme
            | ureq::ErrorKind::InvalidProxyUrl => TransportError::InvalidUrl {
                url: redact_url(url),
                reason: transport.kind().to_string(),
            },
            ureq::ErrorKind::BadHeader => TransportError::InvalidHeader {
                header: "request".to_owned(),
                value: String::new(),
            },
            _ => TransportError::Request {
                url: redact_url(url),
                message: transport
                    .message()
                    .map(str::to_owned)
                    .unwrap_or_else(|| transport.kind().to_string()),
            },
        }),
    }
}

/// Dials each address in turn and returns the first stream that connects; on total failure
/// returns the last error. An empty list is `NotFound`.
///
/// `timeout` is shared by all the addresses: each gets what is left of it, so a name that
/// resolves to several blackholed addresses costs one connect timeout, not one per address
/// (audit-concurrency-1103 crash M2). Once it is used up the rest are not dialled (`TimedOut`).
fn connect_any(addrs: &[SocketAddr], timeout: Duration) -> std::io::Result<TcpStream> {
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses resolved");
    let deadline = Instant::now().checked_add(timeout);
    for addr in addrs {
        let remaining = match deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => timeout,
        };
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connect timeout used up before every address was tried",
            ));
        }
        match TcpStream::connect_timeout(addr, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Whether retrying the same request could plausibly succeed: network-level failures yes,
/// configuration errors (bad URL or header, no TLS backend, disabled, remote cleartext, too
/// large) no.
pub fn is_retriable(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Connect { .. } | TransportError::Io { .. } | TransportError::Request { .. }
    )
}

/// The transport seam the report outbox (`crate::outbox`, ADR-8402) sends its parts through: one
/// `text/plain` POST per part, the part's name in the [`REPORT_NAME_HEADER`] header. The body is
/// already scrubbed and capped by the caller. [`HttpTransport`] is the real one; tests implement
/// the trait with a fake that records parts and scripts answers, with no network.
pub trait UploadTransport {
    /// Sends one part and returns the HTTP status (any 2xx confirms the part).
    ///
    /// # Errors
    /// The part could not be sent or no status came back; the caller resends it later.
    fn post_part(&mut self, name: &str, body: &[u8]) -> Result<u16, TransportError>;
}

/// [`UploadTransport`] over [`post`]: the endpoint's scheme picks the plain or the TLS transport,
/// with the same header validation, credential refusal and `tls`-feature rule as every other
/// upload.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    /// The endpoint every part is POSTed to.
    pub endpoint: String,
    /// Per-attempt timeouts.
    pub timeouts: TransportTimeouts,
}

impl HttpTransport {
    /// A transport to `endpoint` with the default timeouts.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeouts: TransportTimeouts::default(),
        }
    }
}

impl UploadTransport for HttpTransport {
    fn post_part(&mut self, name: &str, body: &[u8]) -> Result<u16, TransportError> {
        post(&self.endpoint, body, "text/plain", Some(name), self.timeouts)
    }
}

/// The delay before retry number `attempt` (1-based): `backoff * 2^(attempt-1)`, saturating, and
/// never longer than five minutes so a large `max_retries` cannot wedge the worker.
pub fn retry_delay(backoff: Duration, attempt: u32) -> Duration {
    const CAP: Duration = Duration::from_secs(300);
    let factor = 1u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    backoff.saturating_mul(factor).min(CAP)
}

/// The dependency-free `http://` transport.
fn post_http(
    url: &str,
    body: &[u8],
    content_type: &str,
    name: Option<&str>,
    timeouts: TransportTimeouts,
) -> Result<u16, TransportError> {
    let endpoint = parse_endpoint(url)?;
    let host = format!("{}:{}", endpoint.host, endpoint.port);

    let addrs: Vec<SocketAddr> = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(|source| TransportError::Connect {
            host: host.clone(),
            source,
        })?
        .collect();
    let mut stream = connect_any(&addrs, timeouts.connect).map_err(|source| TransportError::Connect {
        host: host.clone(),
        source,
    })?;
    stream
        .set_read_timeout(Some(timeouts.read))
        .map_err(|source| TransportError::Io {
            host: host.clone(),
            source,
        })?;
    stream
        .set_write_timeout(Some(timeouts.write))
        .map_err(|source| TransportError::Io {
            host: host.clone(),
            source,
        })?;

    // `None` when the timeout is too large to add to now (`Duration::MAX` = no limit): adding it
    // unchecked panicked, an abort under the shipping profile (audit-concurrency-1103 crash L1).
    let deadline = Instant::now().checked_add(timeouts.write.saturating_mul(SEND_DEADLINE_FACTOR));
    let bare = if endpoint.host.contains(':') {
        format!("[{}]", endpoint.host)
    } else {
        endpoint.host.clone()
    };
    let host_header = if endpoint.port == 80 {
        bare
    } else {
        format!("{bare}:{}", endpoint.port)
    };
    let mut head = format!(
        "POST {} HTTP/1.1\r\nHost: {host_header}\r\nContent-Type: {content_type}\r\n",
        endpoint.path
    );
    if let Some(name) = name {
        head.push_str(&format!("{REPORT_NAME_HEADER}: {name}\r\n"));
        head.push_str(&format!("{LEGACY_REPORT_NAME_HEADER}: {name}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    // One deadline across the request line, headers and body (the headers go through the same
    // bounded writer, so a peer that reads a byte at a time cannot stall them either).
    let mut expired = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
    write_all_bounded(&mut stream, head.as_bytes(), &mut expired)
        .and_then(|()| write_all_bounded(&mut stream, body, &mut expired))
        .and_then(|()| stream.flush())
        .map_err(|source| TransportError::Io {
            host: host.clone(),
            source,
        })?;

    // The read timeout is per read, so one overall deadline (from the end of the send) covers the
    // status line and every header: a peer that trickles response bytes cannot hold the worker.
    let read_deadline = Instant::now().checked_add(timeouts.read);
    let mut reader = BufReader::new(DeadlineReader {
        inner: &mut stream,
        expired: || read_deadline.is_some_and(|deadline| Instant::now() >= deadline),
    });
    read_response_status(&mut reader).map_err(|source| TransportError::Io { host, source })
}

/// Most `1xx` interim responses skipped before the final one.
const MAX_INTERIM_RESPONSES: usize = 4;

/// A reader that fails with `TimedOut` once `expired` reports true, checked before every read.
struct DeadlineReader<R, F> {
    inner: R,
    expired: F,
}

impl<R: Read, F: FnMut() -> bool> Read for DeadlineReader<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if (self.expired)() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "response deadline exceeded",
            ));
        }
        self.inner.read(buf)
    }
}

/// Reads the final response's status code. `1xx` interim responses are skipped (at most
/// `MAX_INTERIM_RESPONSES`). Each response's header block must end with a blank line: a status
/// line followed by EOF, a reset, or the byte cap is an error, not a success, so a collector that
/// died mid-response does not make the reporter delete its only local copy.
fn read_response_status<R: BufRead>(reader: &mut R) -> std::io::Result<u16> {
    for _ in 0..=MAX_INTERIM_RESPONSES {
        let status_line = read_status_line(reader)?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "no status code in response")
            })?;
        let mut headers = reader.by_ref().take(MAX_DRAIN_BYTES);
        let mut line = Vec::new();
        loop {
            line.clear();
            if headers.read_until(b'\n', &mut line)? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "response headers ended before the blank line",
                ));
            }
            if line == b"\r\n" || line == b"\n" {
                break;
            }
        }
        if status >= 200 {
            return Ok(status);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "too many interim responses",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_remote_http_counts_as_cleartext() {
        for url in [
            "http://collector.example.com/r",
            "http://10.0.0.5/r",
            "HTTP://192.168.1.2:80/r",
        ] {
            assert!(is_remote_cleartext(url), "{url}");
        }
        for url in [
            "https://collector.example.com/r",
            "http://127.0.0.1:8080/r",
            "http://localhost/r",
            "http://[::1]:9/r",
            "not a url",
        ] {
            assert!(!is_remote_cleartext(url), "{url}");
        }
    }

    #[test]
    fn parses_endpoints() {
        let e = parse_endpoint("http://127.0.0.1:8080/crash").expect("parse");
        assert_eq!(e.host, "127.0.0.1");
        assert_eq!(e.port, 8080);
        assert_eq!(e.path, "/crash");

        let e = parse_endpoint("http://example.com").expect("parse");
        assert_eq!(e.port, 80);
        assert_eq!(e.path, "/");

        assert!(parse_endpoint("https://example.com/crash").is_err());
        assert!(parse_endpoint("ftp://example.com").is_err());
        assert!(parse_endpoint("not-a-url").is_err());
        assert!(parse_endpoint("http://").is_err());
        assert!(parse_endpoint("http://host:99999/x").is_err());
    }

    /// A writer that accepts one byte per call, like a peer that reads very slowly.
    struct OneByte(Vec<u8>);

    impl Write for OneByte {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.push(buf[0]);
            Ok(1)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn slow_peer_hits_the_send_deadline() {
        let mut sink = OneByte(Vec::new());
        let mut checks = 0u32;
        let result = write_all_bounded(&mut sink, &[7u8; 100], &mut || {
            checks += 1;
            checks > 10
        });
        let error = result.expect_err("deadline must stop the send");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(sink.0.len(), 10, "progress before the deadline is kept");
    }

    #[test]
    fn bounded_write_completes_when_not_expired() {
        let mut sink = OneByte(Vec::new());
        write_all_bounded(&mut sink, &[1u8; 40_000], &mut || false).expect("write");
        assert_eq!(sink.0.len(), 40_000);
    }

    #[test]
    fn endpoints_reject_injection_and_userinfo() {
        for url in [
            "http://127.0.0.1/upload\r\nX-Injected: 1",
            "http://127.0.0.1/up\nload",
            "http://host\0/x",
            "http://user:pass@host/x",
            "http://host/with space",
        ] {
            assert!(parse_endpoint(url).is_err(), "{url:?} must be refused");
        }
        let long = format!("http://host/{}", "a".repeat(MAX_TARGET_LEN));
        assert!(parse_endpoint(&long).is_err());
    }

    #[test]
    fn status_line_is_capped() {
        let endless = vec![b'x'; MAX_STATUS_LINE as usize * 2];
        let mut reader = BufReader::new(&endless[..]);
        assert!(read_status_line(&mut reader).is_err());
        let mut reader = BufReader::new(&b"HTTP/1.1 200 OK\r\nrest"[..]);
        assert_eq!(
            read_status_line(&mut reader).expect("line"),
            "HTTP/1.1 200 OK\r\n"
        );
    }

    #[test]
    fn scheme_routing() {
        assert_eq!(scheme_of("http://x/y").expect("http"), Scheme::Http);
        assert_eq!(scheme_of("https://x/y").expect("https"), Scheme::Https);
        assert!(scheme_of("ftp://x/y").is_err());
        assert!(scheme_of("no-scheme").is_err());
    }

    #[test]
    fn header_values_reject_crlf_and_non_token_bytes() {
        // CR/LF injection is refused.
        assert!(matches!(
            validate_header_value(REPORT_NAME_HEADER, "ok\r\nInjected: x"),
            Err(TransportError::InvalidHeader { .. })
        ));
        assert!(matches!(
            validate_header_value(REPORT_NAME_HEADER, "ok\nInjected: x"),
            Err(TransportError::InvalidHeader { .. })
        ));
        assert!(matches!(
            validate_header_value(REPORT_NAME_HEADER, "nul\x00byte"),
            Err(TransportError::InvalidHeader { .. })
        ));
        // Non-ASCII (non-token) bytes are refused.
        assert!(matches!(
            validate_header_value(REPORT_NAME_HEADER, "naïve"),
            Err(TransportError::InvalidHeader { .. })
        ));
        // Plain names and content types pass.
        validate_header_value(REPORT_NAME_HEADER, "crash-2026-10-06_12-34-56.txt").expect("name");
        validate_header_value("Content-Type", "text/plain").expect("content type");
    }

    #[test]
    fn post_rejects_a_bad_report_name_before_connecting() {
        let result = post(
            "http://127.0.0.1:1/upload",
            b"body",
            "text/plain",
            Some("bad\r\nname"),
            TransportTimeouts::default(),
        );
        assert!(matches!(result, Err(TransportError::InvalidHeader { .. })));
    }

    #[test]
    fn post_against_a_local_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String, Vec<u8>)>::new()));
        let recv = received.clone();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut content_length = 0usize;
            let mut name = String::new();
            let mut legacy_name = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).expect("read");
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    break;
                }
                if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                if let Some(v) = trimmed.strip_prefix("X-Regia-Report-Name:") {
                    name = v.trim().to_owned();
                }
                if let Some(v) = trimmed.strip_prefix(&format!("{LEGACY_REPORT_NAME_HEADER}:")) {
                    legacy_name = v.trim().to_owned();
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).expect("body");
            let mut stream = reader.into_inner();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("reply");
            recv.lock().expect("lock").push((name, legacy_name, body));
        });

        let status = post(
            &format!("http://{addr}/crash"),
            b"hello crash",
            "text/plain",
            Some("crash-1.txt"),
            TransportTimeouts::default(),
        )
        .expect("post");
        assert_eq!(status, 200);
        // The server records the request after it replies: wait for it (on Windows the client
        // read the reply first, ADR-4924).
        server.join().expect("server thread");
        let got = received.lock().expect("lock");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "crash-1.txt");
        // ADR-8670: the legacy header still carries the same name for one release.
        assert_eq!(got[0].1, "crash-1.txt");
        assert_eq!(got[0].2, b"hello crash");
    }

    /// `Duration::MAX` is the usual "no limit" spelling; `Instant::now() + Duration::MAX` used to
    /// panic (an abort under the shipping profile; audit-concurrency-1103 crash L1). Each
    /// unbounded timeout now means no deadline for that phase.
    #[test]
    fn unbounded_timeouts_do_not_panic() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream);
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim_end().is_empty() {
                    break;
                }
                if let Some(v) = line.trim_end().strip_prefix("Content-Length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
            let mut stream = reader.into_inner();
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let status = post(
            &format!("http://{addr}/crash"),
            b"body",
            "text/plain",
            None,
            TransportTimeouts {
                connect: Duration::MAX,
                read: Duration::MAX,
                write: Duration::MAX,
            },
        )
        .expect("post");
        assert_eq!(status, 200);
    }

    #[test]
    fn parses_ipv6_literals_and_rejects_injection() {
        let e = parse_endpoint("http://[::1]:8080/crash").expect("v6 port");
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str()),
            ("::1", 8080, "/crash")
        );
        let e = parse_endpoint("http://[::1]/x").expect("v6 default port");
        assert_eq!((e.host.as_str(), e.port), ("::1", 80));
        assert!(parse_endpoint("http://[::1/x").is_err());
        assert!(parse_endpoint("http://[::1]x/x").is_err());
        assert!(parse_endpoint("http://h/a\r\nX: 1").is_err());
        assert!(parse_endpoint("http://h\r\nX: 1/a").is_err());
        assert!(parse_endpoint("http://h/a b").is_err());
    }

    #[test]
    fn connect_any_skips_a_dead_first_address() {
        let live = std::net::TcpListener::bind("127.0.0.1:0").expect("bind live");
        let live_addr = live.local_addr().expect("addr");
        // A port nothing listens on: bind, note it, drop the listener.
        let dead_addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind dead");
            l.local_addr().expect("addr")
        };
        let stream = connect_any(&[dead_addr, live_addr], Duration::from_secs(5)).expect("falls through");
        assert_eq!(stream.peer_addr().expect("peer"), live_addr);
        assert!(connect_any(&[dead_addr], Duration::from_secs(5)).is_err());
        assert!(connect_any(&[], Duration::from_secs(5)).is_err());
    }

    #[test]
    fn retry_delay_doubles_and_saturates() {
        let b = Duration::from_millis(10);
        assert_eq!(retry_delay(b, 1), b);
        assert_eq!(retry_delay(b, 3), b * 4);
        assert_eq!(retry_delay(b, 32), Duration::from_secs(300));
        assert_eq!(retry_delay(b, u32::MAX), Duration::from_secs(300));
        assert_eq!(retry_delay(Duration::MAX, 5), Duration::from_secs(300));
    }

    #[test]
    fn only_network_errors_are_retriable() {
        let io = || std::io::Error::other("x");
        assert!(is_retriable(&TransportError::Io {
            host: "h".into(),
            source: io()
        }));
        assert!(is_retriable(&TransportError::Connect {
            host: "h".into(),
            source: io()
        }));
        assert!(is_retriable(&TransportError::Request {
            url: "u".into(),
            message: "m".into()
        }));
        assert!(!is_retriable(&TransportError::TlsUnavailable { url: "u".into() }));
        assert!(!is_retriable(&TransportError::InvalidUrl {
            url: "u".into(),
            reason: "r".into()
        }));
        assert!(!is_retriable(&TransportError::Disabled));
        assert!(!is_retriable(&TransportError::RemoteCleartext));
    }

    #[test]
    fn post_returns_without_waiting_for_eof() {
        // The server answers then holds the socket open until the client is done with it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read");
                if line.trim_end().is_empty() {
                    break;
                }
                if let Some(v) = line.trim_end().strip_prefix("Content-Length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).expect("body");
            let mut stream = reader.into_inner();
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                .expect("reply");
            // Keep the socket open until the client has returned.
            let _ = done_rx.recv();
        });
        let timeouts = TransportTimeouts {
            read: Duration::from_secs(600),
            ..TransportTimeouts::default()
        };
        let status = post(&format!("http://{addr}/c"), b"x", "text/plain", None, timeouts).expect("post");
        assert_eq!(status, 204);
        done_tx.send(()).expect("release server");
        server.join().expect("server thread");
    }

    #[test]
    fn response_lines_are_capped() {
        let mut ok = std::io::Cursor::new(b"HTTP/1.1 200 OK\r\nrest".to_vec());
        assert_eq!(
            read_line_capped(&mut ok, 64).expect("line"),
            "HTTP/1.1 200 OK\r\n"
        );
        // No newline within the cap: an error, not an unbounded buffer.
        let mut endless = std::io::Cursor::new(vec![b'a'; 4096]);
        assert!(read_line_capped(&mut endless, 64).is_err());
    }

    #[test]
    fn a_status_line_without_newline_is_an_io_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let junk = vec![b'x'; MAX_STATUS_LINE as usize * 2];
            let _ = stream.write_all(&junk);
        });
        let result = post(
            &format!("http://{addr}/x"),
            b"b",
            "text/plain",
            None,
            TransportTimeouts::default(),
        );
        assert!(matches!(result, Err(TransportError::Io { .. })), "{result:?}");
    }

    #[cfg(not(feature = "tls"))]
    #[test]
    fn https_is_refused_without_the_tls_feature() {
        let result = post(
            "https://crash.example.com/regia",
            b"body",
            "text/plain",
            None,
            TransportTimeouts::default(),
        );
        assert!(matches!(result, Err(TransportError::TlsUnavailable { .. })));
    }

    fn status_of(bytes: &[u8]) -> std::io::Result<u16> {
        read_response_status(&mut BufReader::new(bytes))
    }

    #[test]
    fn a_truncated_header_block_is_an_error_not_a_status() {
        assert_eq!(status_of(b"HTTP/1.1 200 OK\r\nA: b\r\n\r\n").expect("ok"), 200);
        assert_eq!(status_of(b"HTTP/1.1 204 No Content\n\n").expect("ok"), 204);
        // FIN right after the status line, or in the middle of the headers.
        assert!(status_of(b"HTTP/1.1 200 OK\r\n").is_err());
        assert!(status_of(b"HTTP/1.1 200 OK\r\nA: b\r\n").is_err());
        // Header block over the byte cap without a blank line.
        let mut long = b"HTTP/1.1 200 OK\r\n".to_vec();
        long.extend(std::iter::repeat_n(b'a', MAX_DRAIN_BYTES as usize + 10));
        assert!(status_of(&long).is_err());
    }

    #[test]
    fn interim_responses_are_skipped_and_capped() {
        assert_eq!(
            status_of(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\n\r\n").expect("ok"),
            201
        );
        let endless = b"HTTP/1.1 100 Continue\r\n\r\n".repeat(MAX_INTERIM_RESPONSES + 2);
        assert!(status_of(&endless).is_err());
    }

    #[test]
    fn a_response_deadline_stops_a_trickling_peer() {
        /// Yields one byte per read, forever, with no newline.
        struct Trickle;
        impl Read for Trickle {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf[0] = b'x';
                Ok(1)
            }
        }
        let mut reads = 0u32;
        let mut reader = BufReader::new(DeadlineReader {
            inner: Trickle,
            expired: || {
                reads += 1;
                reads > 50
            },
        });
        let error = read_response_status(&mut reader).expect_err("deadline");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn credentials_in_the_url_are_refused_and_redacted() {
        let url = "https://user:secret@crash.example/regia?token=abc";
        let error = reject_userinfo(url).expect_err("userinfo");
        assert!(!error.to_string().contains("secret"), "{error}");
        assert!(!error.to_string().contains("token"), "{error}");
        let error = post(url, b"x", "text/plain", None, TransportTimeouts::default()).expect_err("refused");
        assert!(matches!(error, TransportError::InvalidUrl { .. }), "{error}");
        assert!(!error.to_string().contains("secret"), "{error}");
        assert_eq!(redact_url(url), "https://crash.example");
        assert!(reject_userinfo("http://crash.example/a@b").is_ok());
    }
}
