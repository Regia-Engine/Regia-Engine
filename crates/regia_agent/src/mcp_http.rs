// SPDX-License-Identifier: MPL-2.0
//! The request side of `regia mcp --http` (`docs/specs/23-agent-native.md` § 2, ADR-2346): the
//! HTTP/1.1 request-head parser and the admission policy of the MCP Streamable HTTP endpoint.
//!
//! The server itself lives in `regia_cli` (it needs the async runtime `rmcp` runs on); everything
//! that judges untrusted bytes is here, without IO, so it is unit-tested and fuzzed
//! (`agent.mcp_http_request`) in the crate that owns the protocol's other parsers.
//!
//! Policy, in the order [`admit`] applies it:
//!
//! 1. **Path**: only the endpoint path (default `/mcp`) answers; anything else is `404`.
//! 2. **Method**: `POST` carries one JSON-RPC message; `DELETE` ends a session; `GET` (the optional
//!    server-to-client event stream) is `405`, which the MCP transport allows. Others are `405`.
//! 3. **Origin** (DNS-rebinding protection, required by the MCP transport): a request with an
//!    `Origin` header is accepted only when it names a loopback host (`localhost`, `127.0.0.1`,
//!    `[::1]`) or one of the origins the owner listed (`--allow-origin`); a request without one
//!    (a non-browser client) is accepted.
//! 4. **Host**: on a loopback bind the `Host` header must name a loopback host too, so a rebound
//!    DNS name cannot reach the port.
//! 5. **Token**: `Authorization: Bearer <token>`, always; compared in constant time. `401` with
//!    `WWW-Authenticate: Bearer` otherwise.
//! 6. **Size**: a head over [`MAX_HEAD_BYTES`] is `431`; a `POST` body must have a
//!    `Content-Length` (`411`; chunked bodies are refused) of at most the configured cap (`413`).
//! 7. **Type**: a `POST` body must be `application/json` (`415`).
//!
//! The body is one JSON-RPC message ([`classify_body`]); a batch (JSON array) is `400`: the current
//! MCP revision dropped batching, and one message per request keeps the size cap meaningful.

use serde_json::Value;

/// Most bytes of a request head (request line plus headers).
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Most header lines of one request.
pub const MAX_HEADERS: usize = 64;

/// Default cap on a `POST` body (`--http-max-body` changes it).
pub const DEFAULT_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// The endpoint path.
pub const DEFAULT_PATH: &str = "/mcp";

/// The session header of the MCP Streamable HTTP transport.
pub const SESSION_HEADER: &str = "mcp-session-id";

/// A parsed request head. Header names are lowercased; values are trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    /// `GET`, `POST`, ...
    pub method: String,
    /// The request target as sent (path and query).
    pub target: String,
    /// Headers in order, names lowercased.
    pub headers: Vec<(String, String)>,
}

/// Why a request is refused: the HTTP status and a lowercase reason code (the response body's
/// `reason`, documented in `book/agents/mcp-http.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRefusal {
    /// HTTP status code.
    pub status: u16,
    /// Lowercase reason code.
    pub reason: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl HttpRefusal {
    fn new(status: u16, reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            message: message.into(),
        }
    }

    /// The JSON body sent with the refusal.
    #[must_use]
    pub fn body(&self) -> String {
        serde_json::json!({ "error": { "reason": self.reason, "message": self.message } }).to_string()
    }
}

/// The reason phrase of the statuses this endpoint answers.
#[must_use]
pub fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// Where the head ends in `buf`: the index just past the blank line, or `None` while incomplete.
#[must_use]
pub fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|at| at + 4)
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Parses a request head (the bytes up to and including the blank line).
///
/// # Errors
/// `431 head_too_large`, `400 bad_request` for a malformed line, `505`-class versions answer
/// `400 bad_version`.
pub fn parse_head(head: &[u8]) -> Result<RequestHead, HttpRefusal> {
    if head.len() > MAX_HEAD_BYTES {
        return Err(HttpRefusal::new(
            431,
            "head_too_large",
            format!("the request head is over {MAX_HEAD_BYTES} bytes"),
        ));
    }
    let text = std::str::from_utf8(head)
        .map_err(|_| HttpRefusal::new(400, "bad_request", "the head is not UTF-8"))?;
    let text = text.strip_suffix("\r\n\r\n").unwrap_or(text);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpRefusal::new(400, "bad_request", "malformed request line"));
    };
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(HttpRefusal::new(400, "bad_request", "malformed method"));
    }
    if !target.starts_with('/') || target.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(HttpRefusal::new(400, "bad_request", "malformed request target"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpRefusal::new(
            400,
            "bad_version",
            "only HTTP/1.1 and HTTP/1.0 are served",
        ));
    }
    let mut headers = Vec::new();
    for line in lines {
        if headers.len() >= MAX_HEADERS {
            return Err(HttpRefusal::new(
                431,
                "head_too_large",
                format!("more than {MAX_HEADERS} headers"),
            ));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpRefusal::new(400, "bad_request", "malformed header line"));
        };
        if name.is_empty() || !name.bytes().all(is_token_char) {
            return Err(HttpRefusal::new(400, "bad_request", "malformed header name"));
        }
        let value = value.trim_matches([' ', '\t']);
        if value.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7f) {
            return Err(HttpRefusal::new(
                400,
                "bad_request",
                "control character in a header value",
            ));
        }
        headers.push((name.to_ascii_lowercase(), value.to_string()));
    }
    Ok(RequestHead {
        method: method.to_string(),
        target: target.to_string(),
        headers,
    })
}

impl RequestHead {
    /// The first value of header `name` (lowercase).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// How many times header `name` appears.
    fn count(&self, name: &str) -> usize {
        self.headers.iter().filter(|(n, _)| n == name).count()
    }

    /// The path part of the target (without the query).
    #[must_use]
    pub fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map_or(self.target.as_str(), |(p, _)| p)
    }

    /// The canonical head text: the request line and the headers as parsed, CRLF-separated.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = format!("{} {} HTTP/1.1\r\n", self.method, self.target);
        for (name, value) in &self.headers {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out
    }

    /// Whether the client asked to close the connection after this request.
    #[must_use]
    pub fn wants_close(&self) -> bool {
        self.header("connection")
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")))
    }
}

/// The endpoint's admission policy.
#[derive(Debug, Clone)]
pub struct HttpPolicy {
    /// The bearer token every request must carry.
    pub token: String,
    /// The endpoint path, e.g. `/mcp`.
    pub path: String,
    /// Whether the server is bound to a loopback address (then `Host` must be loopback too).
    pub loopback_bind: bool,
    /// Extra accepted origins (exact `scheme://host[:port]`, lowercase).
    pub allowed_origins: Vec<String>,
    /// Most bytes of a `POST` body.
    pub max_body: usize,
}

/// What an admitted request is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admitted {
    /// A `POST` with a JSON body of this many bytes.
    Post {
        /// The body length (`Content-Length`).
        body_len: usize,
    },
    /// A `DELETE` ending the session in the session header.
    Delete,
}

/// Whether `host` (an authority without userinfo: `name[:port]` or `[v6][:port]`) is a loopback name.
#[must_use]
pub fn is_loopback_authority(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // Only a port may follow the bracket: `[::1].evil.example` names another host.
        match rest.split_once(']') {
            Some((h, "")) => h,
            Some((h, tail))
                if tail
                    .strip_prefix(':')
                    .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit())) =>
            {
                h
            }
            _ => "",
        }
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, port)| {
            if port.bytes().all(|b| b.is_ascii_digit()) {
                h
            } else {
                authority
            }
        })
    };
    let host = host.to_ascii_lowercase();
    host == "localhost" || host == "::1" || host == "127.0.0.1" || host.ends_with(".localhost")
}

/// Whether an `Origin` header value is accepted.
#[must_use]
pub fn origin_allowed(origin: &str, allowed: &[String]) -> bool {
    let origin = origin.trim().to_ascii_lowercase();
    if allowed.iter().any(|a| a.trim_end_matches('/') == origin) {
        return true;
    }
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    !rest.contains(['/', '@', '?', '#']) && is_loopback_authority(rest)
}

/// Compares two byte strings in time that depends on their lengths only.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Applies the policy to a parsed head (see the module docs for the order).
///
/// # Errors
/// The first rule the request breaks.
pub fn admit(policy: &HttpPolicy, head: &RequestHead) -> Result<Admitted, HttpRefusal> {
    if head.path() != policy.path {
        return Err(HttpRefusal::new(
            404,
            "not_found",
            format!("the MCP endpoint is {}", policy.path),
        ));
    }
    match head.method.as_str() {
        "POST" | "DELETE" => {}
        "GET" => {
            return Err(HttpRefusal::new(
                405,
                "no_event_stream",
                "this server answers each POST with JSON and offers no server-to-client stream",
            ));
        }
        _ => {
            return Err(HttpRefusal::new(
                405,
                "method_not_allowed",
                "use POST (or DELETE to end a session)",
            ));
        }
    }
    if head.count("origin") > 1 {
        return Err(HttpRefusal::new(
            403,
            "origin_forbidden",
            "more than one Origin header",
        ));
    }
    if let Some(origin) = head.header("origin")
        && !origin_allowed(origin, &policy.allowed_origins)
    {
        return Err(HttpRefusal::new(
            403,
            "origin_forbidden",
            format!("origin {origin} is not allowed (loopback origins and --allow-origin entries are)"),
        ));
    }
    if policy.loopback_bind {
        let host = head.header("host").unwrap_or_default();
        if head.count("host") != 1 || !is_loopback_authority(host) {
            return Err(HttpRefusal::new(
                403,
                "host_forbidden",
                "on a loopback bind the Host header must name localhost, 127.0.0.1 or [::1]",
            ));
        }
    }
    let presented = head
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::trim);
    let authorized = head.count("authorization") == 1
        && presented.is_some_and(|t| {
            !policy.token.is_empty() && constant_time_eq(t.as_bytes(), policy.token.as_bytes())
        });
    if !authorized {
        return Err(HttpRefusal::new(
            401,
            "unauthorized",
            "send Authorization: Bearer <token> (the token file is named in .regia/agent/mcp_http.json)",
        ));
    }
    if head.method == "DELETE" {
        return Ok(Admitted::Delete);
    }
    if head.header("transfer-encoding").is_some() {
        return Err(HttpRefusal::new(
            411,
            "length_required",
            "chunked bodies are not accepted; send Content-Length",
        ));
    }
    let lengths: Vec<&str> = head
        .headers
        .iter()
        .filter(|(n, _)| n == "content-length")
        .map(|(_, v)| v.as_str())
        .collect();
    let [length] = lengths.as_slice() else {
        return Err(HttpRefusal::new(
            411,
            "length_required",
            "send exactly one Content-Length",
        ));
    };
    let Ok(body_len) = length.parse::<usize>() else {
        return Err(HttpRefusal::new(
            400,
            "bad_request",
            "Content-Length is not a number",
        ));
    };
    if body_len > policy.max_body {
        return Err(HttpRefusal::new(
            413,
            "body_too_large",
            format!("the body is {body_len} bytes; at most {}", policy.max_body),
        ));
    }
    let json = head.header("content-type").is_some_and(|v| {
        v.split(';')
            .next()
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
    });
    if !json {
        return Err(HttpRefusal::new(
            415,
            "not_json",
            "Content-Type must be application/json",
        ));
    }
    if let Some(accept) = head.header("accept") {
        let ok = accept.split(',').any(|t| {
            let t = t.split(';').next().unwrap_or_default().trim();
            t.eq_ignore_ascii_case("application/json")
                || t == "*/*"
                || t.eq_ignore_ascii_case("application/*")
        });
        if !ok {
            return Err(HttpRefusal::new(
                406,
                "not_acceptable",
                "this server answers application/json",
            ));
        }
    }
    Ok(Admitted::Post { body_len })
}

/// What one POSTed JSON-RPC message is.
#[derive(Debug, Clone, PartialEq)]
pub enum BodyKind {
    /// A request: answered with its response. `initialize` opens a session.
    Request {
        /// The request id.
        id: Value,
        /// Whether it is `initialize`.
        initialize: bool,
    },
    /// A notification: answered `202 Accepted`.
    Notification,
    /// A response to a server request: answered `202 Accepted`.
    Response,
}

/// Classifies a POST body as one JSON-RPC message.
///
/// # Errors
/// `400 not_json` for invalid JSON, `400 batch_unsupported` for an array, `400 not_jsonrpc` for
/// anything that is not a JSON-RPC 2.0 message.
pub fn classify_body(body: &[u8]) -> Result<(BodyKind, Value), HttpRefusal> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| HttpRefusal::new(400, "not_json", format!("the body is not JSON: {e}")))?;
    if value.is_array() {
        return Err(HttpRefusal::new(
            400,
            "batch_unsupported",
            "send one JSON-RPC message per request",
        ));
    }
    let Some(object) = value.as_object() else {
        return Err(HttpRefusal::new(
            400,
            "not_jsonrpc",
            "the body must be a JSON-RPC 2.0 object",
        ));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(HttpRefusal::new(
            400,
            "not_jsonrpc",
            "\"jsonrpc\": \"2.0\" is required",
        ));
    }
    let id = object
        .get("id")
        .filter(|id| id.is_string() || id.is_i64() || id.is_u64())
        .cloned();
    let kind = match (object.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => BodyKind::Request {
            id,
            initialize: method == "initialize",
        },
        (Some(_), None) if object.get("id").is_none() => BodyKind::Notification,
        (None, Some(_)) if object.contains_key("result") || object.contains_key("error") => {
            BodyKind::Response
        }
        _ => {
            return Err(HttpRefusal::new(
                400,
                "not_jsonrpc",
                "not a request, notification or response",
            ));
        }
    };
    Ok((kind, value))
}

/// Fuzz loader (`agent.mcp_http_request`): parses a head, applies a fixed policy, classifies the
/// body, and re-encodes the head canonically.
///
/// # Errors
/// The refusal, as text.
pub fn fuzz_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let end = head_end(bytes).ok_or_else(|| "incomplete head".to_string())?;
    let head = parse_head(&bytes[..end]).map_err(|r| r.reason.to_string())?;
    let policy = HttpPolicy {
        token: "fuzz-token".into(),
        path: DEFAULT_PATH.into(),
        loopback_bind: true,
        allowed_origins: vec!["https://tools.example".into()],
        max_body: 1 << 16,
    };
    if let Ok(Admitted::Post { body_len }) = admit(&policy, &head) {
        let body = bytes
            .get(end..end.saturating_add(body_len))
            .unwrap_or(&bytes[end..]);
        let _ = classify_body(body);
    }
    Ok(head.encode().into_bytes())
}

/// The fuzz fixture: a valid `tools/list` POST.
#[must_use]
pub fn fuzz_fixture() -> Vec<u8> {
    let body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
    format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:8931\r\nAuthorization: Bearer fuzz-token\r\n\
         Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         Mcp-Session-Id: abc\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HttpPolicy {
        HttpPolicy {
            token: "s3cret".into(),
            path: "/mcp".into(),
            loopback_bind: true,
            allowed_origins: vec!["https://ide.example".into()],
            max_body: 1024,
        }
    }

    fn head(extra: &str) -> RequestHead {
        let text = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:9000\r\nAuthorization: Bearer s3cret\r\n\
             Content-Type: application/json\r\nContent-Length: 10\r\n{extra}\r\n"
        );
        parse_head(text.as_bytes()).expect("parses")
    }

    fn refused(head: &RequestHead) -> &'static str {
        admit(&policy(), head).err().map_or("admitted", |r| r.reason)
    }

    #[test]
    fn a_well_formed_post_is_admitted() {
        assert_eq!(admit(&policy(), &head("")), Ok(Admitted::Post { body_len: 10 }));
        assert_eq!(
            admit(
                &policy(),
                &head("Origin: http://localhost:3000\r\nAccept: application/json, text/event-stream\r\n")
            ),
            Ok(Admitted::Post { body_len: 10 })
        );
        assert!(admit(&policy(), &head("Origin: https://ide.example\r\n")).is_ok());
    }

    #[test]
    fn foreign_origins_and_hosts_are_refused() {
        assert_eq!(
            refused(&head("Origin: http://evil.example\r\n")),
            "origin_forbidden"
        );
        assert_eq!(
            refused(&head("Origin: http://localhost.evil.example\r\n")),
            "origin_forbidden"
        );
        assert_eq!(
            refused(&head("Origin: http://127.0.0.1@evil.example\r\n")),
            "origin_forbidden"
        );
        assert_eq!(refused(&head("Origin: null\r\n")), "origin_forbidden");
        assert_eq!(
            refused(&head(
                "Origin: http://localhost\r\nOrigin: http://evil.example\r\n"
            )),
            "origin_forbidden"
        );
        let rebound = parse_head(
            b"POST /mcp HTTP/1.1\r\nHost: attacker.example:9000\r\nAuthorization: Bearer s3cret\r\nContent-Type: application/json\r\nContent-Length: 1\r\n\r\n",
        )
        .expect("parses");
        assert_eq!(refused(&rebound), "host_forbidden");
        let remote = HttpPolicy {
            loopback_bind: false,
            ..policy()
        };
        assert!(
            admit(&remote, &rebound).is_ok(),
            "a non-loopback bind does not judge Host"
        );
    }

    #[test]
    fn the_token_is_required_and_compared_exactly() {
        let no_token = parse_head(
            b"POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 1\r\n\r\n",
        )
        .expect("parses");
        assert_eq!(refused(&no_token), "unauthorized");
        let wrong = parse_head(
            b"POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer s3creT\r\nContent-Type: application/json\r\nContent-Length: 1\r\n\r\n",
        )
        .expect("parses");
        assert_eq!(refused(&wrong), "unauthorized");
        assert_eq!(
            refused(&head("Authorization: Bearer s3cret\r\n")),
            "unauthorized",
            "two tokens"
        );
        let empty = HttpPolicy {
            token: String::new(),
            ..policy()
        };
        assert!(admit(&empty, &head("")).is_err(), "an empty token never matches");
    }

    #[test]
    fn size_type_and_method_rules_hold() {
        let big = parse_head(
            b"POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer s3cret\r\nContent-Type: application/json\r\nContent-Length: 1025\r\n\r\n",
        )
        .expect("parses");
        assert_eq!(refused(&big), "body_too_large");
        assert_eq!(
            refused(&head("Transfer-Encoding: chunked\r\n")),
            "length_required"
        );
        assert_eq!(
            refused(&head("Content-Length: 10\r\n")),
            "length_required",
            "two lengths"
        );
        assert_eq!(refused(&head("Accept: text/html\r\n")), "not_acceptable");
        let text = parse_head(
            b"POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer s3cret\r\nContent-Type: text/plain\r\nContent-Length: 1\r\n\r\n",
        )
        .expect("parses");
        assert_eq!(refused(&text), "not_json");
        let get = parse_head(b"GET /mcp HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("parses");
        assert_eq!(refused(&get), "no_event_stream");
        let other = parse_head(b"POST /other HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("parses");
        assert_eq!(refused(&other), "not_found");
        let delete =
            parse_head(b"DELETE /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer s3cret\r\n\r\n")
                .expect("parses");
        assert_eq!(admit(&policy(), &delete), Ok(Admitted::Delete));
    }

    #[test]
    fn malformed_heads_are_refused() {
        for bad in [
            &b"POST /mcp\r\n\r\n"[..],
            b"post /mcp HTTP/1.1\r\n\r\n",
            b"POST mcp HTTP/1.1\r\n\r\n",
            b"POST /mcp HTTP/2\r\n\r\n",
            b"POST /mcp HTTP/1.1\r\nno colon\r\n\r\n",
            b"POST /mcp HTTP/1.1\r\nbad name: x\r\n\r\n",
            b"POST /mcp HTTP/1.1\r\nX: a\x01b\r\n\r\n",
            b"POST /m cp HTTP/1.1\r\n\r\n",
        ] {
            assert!(parse_head(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let many: String = (0..=MAX_HEADERS).map(|i| format!("X-{i}: v\r\n")).collect();
        let text = format!("POST /mcp HTTP/1.1\r\n{many}\r\n");
        assert_eq!(parse_head(text.as_bytes()).map_err(|r| r.status), Err(431));
        assert_eq!(
            parse_head(&vec![b'a'; MAX_HEAD_BYTES + 1]).map_err(|r| r.status),
            Err(431)
        );
    }

    #[test]
    fn bodies_classify_as_one_jsonrpc_message() {
        let (kind, _) =
            classify_body(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).expect("request");
        assert_eq!(
            kind,
            BodyKind::Request {
                id: serde_json::json!(1),
                initialize: true
            }
        );
        let (kind, _) =
            classify_body(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).expect("note");
        assert_eq!(kind, BodyKind::Notification);
        let (kind, _) = classify_body(br#"{"jsonrpc":"2.0","id":"a","result":{}}"#).expect("response");
        assert_eq!(kind, BodyKind::Response);
        let reason = |b: &[u8]| classify_body(b).err().map(|r| r.reason);
        assert_eq!(reason(b"[]"), Some("batch_unsupported"));
        assert_eq!(reason(b"{"), Some("not_json"));
        assert_eq!(reason(br#"{"id":1,"method":"x"}"#), Some("not_jsonrpc"));
        assert_eq!(
            reason(br#"{"jsonrpc":"2.0","id":null,"method":"x"}"#),
            Some("not_jsonrpc")
        );
        assert_eq!(
            reason(br#"{"jsonrpc":"2.0","id":1.5,"method":"x"}"#),
            Some("not_jsonrpc")
        );
    }

    #[test]
    fn loopback_names_are_recognised() {
        for yes in [
            "localhost",
            "localhost:80",
            "127.0.0.1:8931",
            "[::1]:9",
            "[::1]",
            "app.localhost",
        ] {
            assert!(is_loopback_authority(yes), "{yes}");
        }
        for no in [
            "example.com",
            "127.0.0.2",
            "localhost.example",
            "[::2]:1",
            "[::1].evil.example",
            "[::1].evil.example:80",
            "[::1]x",
            "[::1]:8x",
            "[::1",
            "",
            "0.0.0.0:1",
        ] {
            assert!(!is_loopback_authority(no), "{no}");
        }
    }

    #[test]
    fn the_fixture_loads_and_round_trips() {
        let encoded = fuzz_load(&fuzz_fixture()).expect("loads");
        assert_eq!(fuzz_load(&encoded).expect("reloads"), encoded);
        let mut rng = regia_core::fuzz::FuzzRng::new(0x2319_0002);
        let fixture = fuzz_fixture();
        for _ in 0..3000 {
            let mutated = regia_core::fuzz::mutate_bytes(&fixture, &mut rng);
            if let Ok(encoded) = fuzz_load(&mutated) {
                assert_eq!(fuzz_load(&encoded).as_ref(), Ok(&encoded), "round trip");
            }
        }
    }
}
