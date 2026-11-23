// SPDX-License-Identifier: MPL-2.0
//! A shared cook cache behind an HTTP server (ADR-2612, ADR-2710): `GET`, `HEAD`, `PUT` and
//! `DELETE` of `<base>/v1/<aa>/<key hex>.rcc`, the directory backend's layout, one request per
//! connection. `http://` goes over a small HTTP/1.1 client on `std::net`; `https://` goes through
//! the workspace's `ureq` + rustls stack (`ring` provider, web PKI roots, no redirects). Any server
//! that stores `PUT` bodies whole before it serves them works (a WebDAV-style static server writes
//! a temp file and renames it), so a reader sees a whole entry or none, as with the directory
//! backend; every entry read is still verified ([`super::verify_entry`]). Listing (`prune`,
//! `stats`) is the server's job: [`CookCacheBackend::list`] answers `Unsupported`.
//!
//! **Auth.** When [`SHARED_CACHE_AUTH_ENV`] is set its value is sent as the `Authorization` header
//! of every request (`Bearer <token>`, `Basic <base64>`). The value is never logged, printed or
//! put in an error, and it is never sent over plain `http://` to another machine.
//!
//! Request and response framing follow RFC 9112 (HTTP/1.1 message syntax: status line, header
//! fields, `Content-Length` or `chunked` bodies). Original code.
//!
//! **Features.** `https://` is the `http_cache` feature (on by default, ADR-7972). Without it the
//! crate links no TLS stack: `http://` and directory caches work as before, and an `https://` URL is
//! refused when the backend is opened, with [`COOK_CACHE_HTTPS_DISABLED_CODE`] at the start of the
//! message (never a silent fallback to plain `http://`).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use super::{CacheEntryInfo, CookCacheBackend, ENTRY_EXT, LAYOUT_DIR, hex};

/// Connect timeout of one request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Read and write timeout of one request.
const IO_TIMEOUT: Duration = Duration::from_secs(120);
/// The largest response header block read.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// The largest entry body accepted (a source's cooked outputs).
const MAX_BODY_BYTES: u64 = 8 << 30;

/// The environment variable whose value is sent as the `Authorization` header to an `http(s)://`
/// shared cook cache (for example `Bearer <token>`). Never logged.
pub const SHARED_CACHE_AUTH_ENV: &str = "REGIA_SHARED_COOK_CACHE_AUTH";

/// The error code an `https://` shared cook cache fails with in a build without the `http_cache`
/// feature (ADR-7972).
pub const COOK_CACHE_HTTPS_DISABLED_CODE: &str = "cook_cache_https_disabled";

/// The `https://` client: ureq's agent with the `http_cache` feature (ADR-7972).
#[cfg(feature = "http_cache")]
type TlsAgent = ureq::Agent;

/// Without the `http_cache` feature no `https://` backend is ever built, so no agent exists.
#[cfg(not(feature = "http_cache"))]
#[derive(Clone)]
enum TlsAgent {}

/// An `Authorization` header value. Its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
struct AuthHeader(String);

impl std::fmt::Debug for AuthHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthHeader(<redacted>)")
    }
}

/// A shared cook cache on an `http://` or `https://` server.
#[derive(Clone)]
pub struct HttpBackend {
    /// `https://` through ureq + rustls.
    tls: bool,
    host: String,
    port: u16,
    /// The path prefix, without a trailing `/` (`""` for the server root).
    base: String,
    /// The `https://` agent.
    agent: Option<TlsAgent>,
    auth: Option<AuthHeader>,
}

impl std::fmt::Debug for HttpBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpBackend")
            .field("location", &self.location())
            .field("auth", &self.auth.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// What one request answered.
struct Reply {
    status: u16,
    body: Vec<u8>,
}

impl HttpBackend {
    /// The cache at `url` (`http://host[:port][/path]` or `https://...`), sending
    /// [`SHARED_CACHE_AUTH_ENV`] as the `Authorization` header when it is set. `https://` trusts
    /// the web PKI roots.
    ///
    /// # Errors
    /// Not an `http://` or `https://` URL, a bad auth value, or auth over plain `http://` to
    /// another machine (the message never contains the value). An `https://` URL in a build
    /// without the `http_cache` feature ([`COOK_CACHE_HTTPS_DISABLED_CODE`], ADR-7972).
    pub fn new(url: &str) -> Result<Self, String> {
        let auth = std::env::var_os(SHARED_CACHE_AUTH_ENV)
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.into_string()
                    .map_err(|_| format!("shared cook cache {url}: {SHARED_CACHE_AUTH_ENV} is not UTF-8"))
            })
            .transpose()?;
        Self::build(url, None, auth)
    }

    /// The cache at `url` with an explicit `https://` agent (`None`: the default one, web PKI
    /// roots) and `Authorization` value (`None`: none). Tools that pin their own roots and tests
    /// use it.
    ///
    /// # Errors
    /// As [`Self::new`].
    ///
    /// The `http_cache` feature (ADR-7972).
    #[cfg(feature = "http_cache")]
    pub fn with_options(url: &str, agent: Option<ureq::Agent>, auth: Option<String>) -> Result<Self, String> {
        Self::build(url, agent, auth)
    }

    /// `Self::with_options` for any build: `agent` is always `None` without `http_cache`.
    fn build(url: &str, agent: Option<TlsAgent>, auth: Option<String>) -> Result<Self, String> {
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(format!("shared cook cache {url}: not an http:// or https:// URL"));
        };
        let default_port = if tls { 443 } else { 80 };
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        if authority.is_empty() || authority.contains(['@', ' ']) {
            return Err(format!("shared cook cache {url}: bad host"));
        }
        let bad_port = || format!("shared cook cache {url}: bad port");
        let (host, port) = if authority.starts_with('[') {
            // A bracketed IPv6 literal, optionally followed by `:port`.
            let end = authority
                .find(']')
                .ok_or_else(|| format!("shared cook cache {url}: bad host"))?;
            let (host, rest) = authority.split_at(end + 1);
            match rest.strip_prefix(':') {
                Some(port) => (host, port.parse::<u16>().map_err(|_| bad_port())?),
                None if rest.is_empty() => (host, default_port),
                None => return Err(format!("shared cook cache {url}: bad host")),
            }
        } else {
            match authority.split_once(':') {
                Some((host, port)) => (host, port.parse::<u16>().map_err(|_| bad_port())?),
                None => (authority, default_port),
            }
        };
        if path.contains(['?', '#', ' ']) || path.split('/').any(|seg| seg == "..") {
            return Err(format!("shared cook cache {url}: the path must be a plain path"));
        }
        let auth = match auth {
            None => None,
            // CR/LF would split the header block; say so without echoing the value.
            Some(v) if v.trim().is_empty() || v.bytes().any(|b| b.is_ascii_control()) => {
                return Err(format!(
                    "shared cook cache {url}: {SHARED_CACHE_AUTH_ENV} holds an empty value or a control character"
                ));
            }
            Some(v) => Some(AuthHeader(v)),
        };
        let loopback = matches!(host, "localhost" | "[::1]") || host.starts_with("127.");
        if auth.is_some() && !tls && !loopback {
            return Err(format!(
                "shared cook cache {url}: {SHARED_CACHE_AUTH_ENV} is set, and credentials are never sent over plain http:// to another machine; use https://"
            ));
        }
        let agent = Self::tls_agent(url, tls, agent)?;
        Ok(Self {
            tls,
            host: host.to_string(),
            port,
            base: path.trim_end_matches('/').to_string(),
            agent,
            auth,
        })
    }

    /// The agent of a parsed URL: none for `http://`, the given one or the default for `https://`.
    #[cfg(feature = "http_cache")]
    fn tls_agent(_url: &str, tls: bool, agent: Option<TlsAgent>) -> Result<Option<TlsAgent>, String> {
        Ok(match (tls, agent) {
            (false, _) => None,
            (true, Some(agent)) => Some(agent),
            (true, None) => Some(
                ureq::AgentBuilder::new()
                    .timeout_connect(CONNECT_TIMEOUT)
                    .timeout_read(IO_TIMEOUT)
                    .timeout_write(IO_TIMEOUT)
                    .user_agent("regia-cook-cache")
                    // A cache never redirects; a redirect must not carry the credentials elsewhere.
                    .redirects(0)
                    .https_only(true)
                    .build(),
            ),
        })
    }

    /// Without the `http_cache` feature an `https://` URL is refused here, when the backend is
    /// opened (ADR-7972); `http://` needs no agent.
    #[cfg(not(feature = "http_cache"))]
    fn tls_agent(url: &str, tls: bool, agent: Option<TlsAgent>) -> Result<Option<TlsAgent>, String> {
        if let Some(agent) = agent {
            match agent {}
        }
        if tls {
            return Err(format!(
                "{COOK_CACHE_HTTPS_DISABLED_CODE}: shared cook cache {url}: https:// is not built in (regia_asset_pipeline feature `http_cache` is off); use http:// on a trusted network or a directory cache"
            ));
        }
        Ok(None)
    }

    /// Whether requests carry an `Authorization` header.
    #[must_use]
    pub fn has_auth(&self) -> bool {
        self.auth.is_some()
    }

    fn entry_target(&self, key: &[u8; 32]) -> String {
        let hex = hex(key);
        format!("{}/{LAYOUT_DIR}/{}/{hex}.{ENTRY_EXT}", self.base, &hex[..2])
    }

    /// `host` or `host:port` (the port only when it is not the scheme's default).
    fn authority(&self) -> String {
        if self.port == if self.tls { 443 } else { 80 } {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Sends one request and reads the whole reply (`HEAD`: no body).
    fn request(&self, method: &str, target: &str, body: Option<&[u8]>) -> io::Result<Reply> {
        if let Some(agent) = &self.agent {
            #[cfg(feature = "http_cache")]
            return self.request_tls(agent, method, target, body);
            #[cfg(not(feature = "http_cache"))]
            match *agent {}
        }
        let addr = (self.host.trim_start_matches('[').trim_end_matches(']'), self.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "host has no address"))?;
        let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.write_all(
            self.request_head(method, target, body.map(<[u8]>::len))
                .as_bytes(),
        )?;
        if let Some(body) = body {
            stream.write_all(body)?;
        }
        stream.flush()?;
        read_reply(BufReader::new(stream), method == "HEAD")
    }

    /// The request line and header block of a plain `http://` request.
    fn request_head(&self, method: &str, target: &str, body_len: Option<usize>) -> String {
        let length = body_len
            .map(|n| format!("Content-Length: {n}\r\n"))
            .unwrap_or_default();
        let auth = self
            .auth
            .as_ref()
            .map(|a| format!("Authorization: {}\r\n", a.0))
            .unwrap_or_default();
        format!(
            "{method} {target} HTTP/1.1\r\nHost: {}\r\n{auth}{length}Connection: close\r\nUser-Agent: regia-cook-cache\r\n\r\n",
            self.authority()
        )
    }

    /// One `https://` request through ureq.
    #[cfg(feature = "http_cache")]
    fn request_tls(
        &self,
        agent: &ureq::Agent,
        method: &str,
        target: &str,
        body: Option<&[u8]>,
    ) -> io::Result<Reply> {
        let mut req = agent.request(method, &format!("https://{}{target}", self.authority()));
        if let Some(auth) = &self.auth {
            req = req.set("Authorization", &auth.0);
        }
        let result = match body {
            Some(body) => req.send_bytes(body),
            None => req.call(),
        };
        let response = match result {
            Ok(response) => response,
            // 4xx/5xx: the caller maps the status (a 401 is an error, a 404 a miss).
            Err(ureq::Error::Status(status, _)) => {
                return Ok(Reply {
                    status,
                    body: Vec::new(),
                });
            }
            // The transport error names the URL and the cause, never a header.
            Err(ureq::Error::Transport(t)) => return Err(io::Error::other(t.to_string())),
        };
        let status = response.status();
        let mut body = Vec::new();
        if method != "HEAD" {
            response
                .into_reader()
                .take(MAX_BODY_BYTES + 1)
                .read_to_end(&mut body)?;
            if body.len() as u64 > MAX_BODY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bad HTTP reply: body too large",
                ));
            }
        }
        Ok(Reply { status, body })
    }
}

/// Reads a status line, header fields and the body (`Content-Length`, `chunked` or to the end).
fn read_reply<R: BufRead>(mut reader: R, head_only: bool) -> io::Result<Reply> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("bad HTTP reply: {why}"));
    let mut seen = 0usize;
    let mut line = String::new();
    let mut next_line = |reader: &mut R, line: &mut String| -> io::Result<()> {
        line.clear();
        let n = reader.by_ref().take(MAX_HEADER_BYTES as u64).read_line(line)?;
        seen += n;
        if n == 0 || seen > MAX_HEADER_BYTES {
            return Err(bad("header block ended early or is too large"));
        }
        Ok(())
    };
    next_line(&mut reader, &mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|_| line.starts_with("HTTP/1."))
        .ok_or_else(|| bad("status line"))?;
    let (mut length, mut chunked) = (None, false);
    loop {
        next_line(&mut reader, &mut line)?;
        let field = line.trim_end_matches(['\r', '\n']);
        if field.is_empty() {
            break;
        }
        if let Some((name, value)) = field.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.parse::<u64>().map_err(|_| bad("content-length"))?);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.to_ascii_lowercase().contains("chunked");
            }
        }
    }
    let mut body = Vec::new();
    if head_only || status == 204 || status == 304 {
        return Ok(Reply { status, body });
    }
    if chunked {
        loop {
            line.clear();
            reader.by_ref().take(1024).read_line(&mut line)?;
            let size = line.trim().split(';').next().unwrap_or("");
            let size = u64::from_str_radix(size, 16).map_err(|_| bad("chunk size"))?;
            if size == 0 {
                break;
            }
            if (body.len() as u64)
                .checked_add(size)
                .is_none_or(|total| total > MAX_BODY_BYTES)
            {
                return Err(bad("body too large"));
            }
            read_exactly(&mut reader, size, &mut body)?;
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf)?;
            if &crlf != b"\r\n" {
                return Err(bad("chunk not followed by CRLF"));
            }
        }
    } else if let Some(length) = length {
        if length > MAX_BODY_BYTES {
            return Err(bad("body too large"));
        }
        read_exactly(&mut reader, length, &mut body)?;
    } else {
        reader.take(MAX_BODY_BYTES + 1).read_to_end(&mut body)?;
        if body.len() as u64 > MAX_BODY_BYTES {
            return Err(bad("body too large"));
        }
    }
    Ok(Reply { status, body })
}

/// Appends exactly `len` bytes of `reader` to `body`. The buffer grows with the bytes that
/// arrive, so a server that announces a huge length and sends little costs little memory.
fn read_exactly<R: Read>(reader: &mut R, len: u64, body: &mut Vec<u8>) -> io::Result<()> {
    let got = reader.by_ref().take(len).read_to_end(body)?;
    if got as u64 == len {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("HTTP body ended after {got} of {len} bytes"),
        ))
    }
}

fn unexpected(method: &str, status: u16) -> io::Error {
    io::Error::other(format!("{method} answered HTTP {status}"))
}

impl CookCacheBackend for HttpBackend {
    fn location(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}{}", self.authority(), self.base)
    }

    fn get(&self, key: &[u8; 32]) -> io::Result<Option<Vec<u8>>> {
        let reply = self.request("GET", &self.entry_target(key), None)?;
        match reply.status {
            200 => Ok(Some(reply.body)),
            404 | 410 => Ok(None),
            status => Err(unexpected("GET", status)),
        }
    }

    fn contains(&self, key: &[u8; 32]) -> io::Result<bool> {
        match self.request("HEAD", &self.entry_target(key), None)?.status {
            200 => Ok(true),
            404 | 410 => Ok(false),
            status => Err(unexpected("HEAD", status)),
        }
    }

    fn put(&self, key: &[u8; 32], bytes: &[u8]) -> io::Result<()> {
        match self.request("PUT", &self.entry_target(key), Some(bytes))?.status {
            200 | 201 | 204 => Ok(()),
            status => Err(unexpected("PUT", status)),
        }
    }

    fn touch(&self, _key: &[u8; 32]) -> io::Result<()> {
        // The server's access log or access times track use; nothing to send.
        Ok(())
    }

    fn remove(&self, key: &[u8; 32]) -> io::Result<()> {
        match self.request("DELETE", &self.entry_target(key), None)?.status {
            200 | 202 | 204 | 404 | 410 => Ok(()),
            status => Err(unexpected("DELETE", status)),
        }
    }

    fn list(&self) -> io::Result<Vec<CacheEntryInfo>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "an http:// shared cook cache cannot be listed; run `regia cook cache prune|stats` on the server's directory",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_into_host_port_and_base() {
        let b = HttpBackend::new("http://cache.lan:8080/regia/cook/").expect("url");
        assert_eq!(
            (b.host.as_str(), b.port, b.base.as_str()),
            ("cache.lan", 8080, "/regia/cook")
        );
        assert_eq!(b.location(), "http://cache.lan:8080/regia/cook");
        let key = [0xab; 32];
        assert_eq!(
            b.entry_target(&key),
            format!("/regia/cook/v1/ab/{}.rcc", hex(&key))
        );
        let root = HttpBackend::new("http://cache").expect("url");
        assert_eq!((root.port, root.base.as_str()), (80, ""));
        assert_eq!(root.location(), "http://cache");
        let v6 = HttpBackend::new("http://[::1]:9000/c").expect("url");
        assert_eq!((v6.host.as_str(), v6.port), ("[::1]", 9000));
        let v6 = HttpBackend::new("http://[::1]/c").expect("url");
        assert_eq!((v6.host.as_str(), v6.port), ("[::1]", 80));
        for bad in [
            "ftp://cache",
            "http://",
            "http://user@cache/",
            "http://cache:port/",
            "http://cache/a/../b",
            "http://cache/a?x=1",
        ] {
            assert!(HttpBackend::new(bad).is_err(), "{bad}");
        }
        // `https://` needs the `http_cache` feature (ADR-7972; the off side is tested in `cook_cache`).
        #[cfg(feature = "http_cache")]
        {
            let tls = HttpBackend::build("https://cache.lan/regia/", None, None).expect("https");
            assert_eq!(
                (tls.port, tls.location()),
                (443, "https://cache.lan/regia".to_owned())
            );
            let tls = HttpBackend::build("https://cache.lan:8443", None, None).expect("https");
            assert_eq!(tls.location(), "https://cache.lan:8443");
        }
    }

    #[test]
    fn auth_is_sent_never_shown_and_never_sent_in_the_clear() {
        let secret = "Bearer s3cr3t-token";
        let b = HttpBackend::build("http://127.0.0.1:9/c", None, Some(secret.into())).expect("loopback");
        assert!(b.has_auth());
        let head = b.request_head("GET", "/c/v1/ab/x.rcc", None);
        assert!(
            head.contains("\r\nAuthorization: Bearer s3cr3t-token\r\n"),
            "{head}"
        );
        assert!(!format!("{b:?}").contains("s3cr3t"), "Debug hides the value");
        #[cfg(feature = "http_cache")]
        {
            let tls = HttpBackend::build("https://cache.lan/c", None, Some(secret.into())).expect("https");
            assert!(!format!("{tls:?}").contains("s3cr3t"));
        }
        // Plain http to another machine: refused, and the message does not echo the value.
        let err = HttpBackend::build("http://cache.lan/c", None, Some(secret.into())).expect_err("cleartext");
        assert!(
            err.contains(SHARED_CACHE_AUTH_ENV) && !err.contains("s3cr3t"),
            "{err}"
        );
        // A value that would split the header block.
        let err = HttpBackend::build("https://cache.lan/c", None, Some("Bearer a\r\nX: y".into()))
            .expect_err("injection");
        assert!(!err.contains("Bearer"), "{err}");
        let plain = HttpBackend::build("http://cache.lan/c", None, None).expect("no auth");
        assert!(!plain.request_head("PUT", "/c", Some(3)).contains("Authorization"));
        assert!(
            plain
                .request_head("PUT", "/c", Some(3))
                .contains("Content-Length: 3\r\n")
        );
    }

    #[test]
    fn replies_are_read_by_length_chunks_or_to_the_end() {
        let read = |text: &[u8], head: bool| read_reply(io::Cursor::new(text.to_vec()), head);
        let r = read(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello trailing",
            false,
        )
        .expect("reply");
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hello"[..]));
        let r = read(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n",
            false,
        )
        .expect("chunked");
        assert_eq!(r.body, b"abcde");
        let r = read(b"HTTP/1.0 404 Not Found\r\n\r\nmissing", false).expect("to the end");
        assert_eq!((r.status, r.body.as_slice()), (404, &b"missing"[..]));
        let r = read(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n", true).expect("head");
        assert!(r.body.is_empty());
        for bad in [
            &b""[..],
            b"garbage\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort",
            b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            b"HTTP/1.1 200 OK\r\n",
            // A chunk size that overflows, one past the cap, a huge announced length with no
            // body, a chunk without its CRLF.
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\nffffffffffffffff\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n200000001\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 8000000000\r\n\r\nab",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXY0\r\n\r\n",
        ] {
            assert!(read(bad, false).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }
}
