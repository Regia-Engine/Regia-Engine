// SPDX-License-Identifier: MPL-2.0
//! A small synchronous client, shared by `regia agent call`, `regia mcp` and tests. It speaks
//! to the loopback WebSocket listener (ADR-014) or, where the endpoint advertises one, the
//! local transport (spec 23 § 2): a Unix domain socket or a Windows named pipe carrying the
//! same JSON-RPC text messages (`crate::local`, book/agents/local-transport.md).

use std::net::TcpStream;
use std::path::Path;

use serde_json::Value;
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

use crate::platform::LocalStream;
use crate::protocol::{Request, Response, RpcError};

/// Client errors.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No running app advertised an endpoint.
    #[error("no running Regia Engine app found ({path}): {source}")]
    NoEndpoint {
        /// Agent directory that was searched.
        path: String,
        /// Why it could not be read.
        source: std::io::Error,
    },
    /// Connecting or a transport failed.
    #[error("connection failed: {0}")]
    Connection(String),
    /// The server answered with a JSON-RPC error.
    #[error("{} ({}): {}", .0.message, .0.code, .0.data.as_ref().map(|d| d.to_string()).unwrap_or_default())]
    Rpc(RpcError),
    /// The answer was not valid JSON-RPC.
    #[error("invalid response: {0}")]
    Protocol(String),
    /// The app did not answer within the read timeout ([`Client::set_read_timeout`]). The
    /// connection stays usable: the next call can be sent (and a late answer is skipped by
    /// request id).
    #[error("no answer within the read timeout")]
    TimedOut,
}

/// The wire the connection rides on. Both transports carry the same JSON-RPC text messages.
enum Transport {
    // The socket is by far the larger variant; box it so `Client` stays small in the local
    // case.
    WebSocket(Box<WebSocket<TcpStream>>),
    Local(LocalStream),
}

/// A connection to an app's agent server.
pub struct Client {
    transport: Transport,
    next_id: u64,
}

impl Client {
    /// Connects to `url` (e.g. `ws://127.0.0.1:41234`) with a bearer token.
    pub fn connect(url: &str, token: &str) -> Result<Self, ClientError> {
        let mut request = url
            .into_client_request()
            .map_err(|e| ClientError::Connection(e.to_string()))?;
        let auth = format!("Bearer {token}")
            .parse()
            .map_err(|_| ClientError::Connection("bad token".into()))?;
        request.headers_mut().insert("authorization", auth);
        let host = request.uri().host().unwrap_or("127.0.0.1").to_string();
        // The server only ever binds loopback and `endpoint.json` admits only `127.0.0.1`;
        // never send the token anywhere else (an `endpoint.json` from an untrusted project could
        // name any host, and `localhost` / `[::1]` resolve through the hosts file / resolver).
        if !matches!(host.as_str(), "127.0.0.1") {
            return Err(ClientError::Connection(format!(
                "refusing to connect to non-loopback host `{host}`"
            )));
        }
        let port = request.uri().port_u16().unwrap_or(80);
        let stream = TcpStream::connect((connection_host(&host), port))
            .map_err(|e| ClientError::Connection(e.to_string()))?;
        let _ = stream.set_nodelay(true);
        let (socket, _) =
            tungstenite::client(request, stream).map_err(|e| ClientError::Connection(e.to_string()))?;
        Ok(Self {
            transport: Transport::WebSocket(Box::new(socket)),
            next_id: 1,
        })
    }

    /// Connects to the local transport at `address` — the absolute socket path (Linux/macOS)
    /// or pipe name (Windows) a `socket` advertisement holds — with a bearer token. The token
    /// goes in the handshake frame, the counterpart of the WebSocket handshake's
    /// `Authorization` header (ADR-5708), and the server answers whether it took it
    /// (ADR-5878): a refused token is an error here, not at the first call. On Windows the
    /// pipe's server process must be the one the pipe name carries before the token is sent,
    /// so a pipe another account created first never receives it.
    ///
    /// # Errors
    ///
    /// [`ClientError::Connection`] when `address` is not a well-formed local-transport address,
    /// nothing listens there, the pipe's server is another process, the server refuses the
    /// token, or no handshake answer arrives within `server::HANDSHAKE_TIMEOUT`.
    pub fn connect_local(address: &str, token: &str) -> Result<Self, ClientError> {
        if !crate::endpoint::valid_socket_address(address, cfg!(windows)) {
            return Err(ClientError::Connection(format!(
                "refusing to connect to `{address}`: not a local transport address"
            )));
        }
        let mut stream = LocalStream::connect(address).map_err(|e| ClientError::Connection(e.to_string()))?;
        let handshake = serde_json::to_vec(&serde_json::json!({ "bearer": token }))
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        let connection = |e: std::io::Error| ClientError::Connection(e.to_string());
        crate::local::write_frame(&mut stream, &handshake).map_err(connection)?;
        let deadline = std::time::Instant::now() + crate::server::HANDSHAKE_TIMEOUT;
        let answer =
            crate::local::read_frame_before(&mut stream, crate::local::MAX_HANDSHAKE_BYTES, deadline)
                .map_err(connection)?;
        stream.set_read_timeout(None).map_err(connection)?;
        if !handshake_accepted(answer.as_deref()) {
            return Err(ClientError::Connection(format!(
                "the local transport at `{address}` refused the bearer token"
            )));
        }
        Ok(Self {
            transport: Transport::Local(stream),
            next_id: 1,
        })
    }

    /// How long a pending answer may take before [`Client::call`] returns
    /// [`ClientError::TimedOut`] instead of blocking (an app frozen mid-frame would otherwise
    /// block the caller forever). `None` (the default) waits indefinitely. Works on every
    /// transport: the WebSocket's TCP socket, the local Unix socket and the Windows named
    /// pipe (overlapped reads, ADR-5878).
    ///
    /// # Errors
    ///
    /// [`ClientError::Connection`] for a zero duration (as `std` refuses one) or when the
    /// socket refuses the timeout.
    pub fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> Result<(), ClientError> {
        match &mut self.transport {
            Transport::WebSocket(socket) => socket
                .get_ref()
                .set_read_timeout(timeout)
                .map_err(|e| ClientError::Connection(e.to_string())),
            Transport::Local(stream) => stream
                .set_read_timeout(timeout)
                .map_err(|e| ClientError::Connection(e.to_string())),
        }
    }

    /// Connects to the app advertised in `<project>/.regia/agent/`, preferring the local
    /// transport when the endpoint advertises one (spec 23 § 2): same messages, no TCP port.
    /// A dead local listener, or a live one that refuses the token (another app's, ADR-5878),
    /// falls back to the WebSocket URL, so a stale `socket` file does not lock clients out.
    pub fn connect_project(project_dir: &Path) -> Result<Self, ClientError> {
        let dir = crate::endpoint::agent_dir(project_dir);
        let (endpoint, token) = crate::endpoint::read(&dir).map_err(|source| ClientError::NoEndpoint {
            path: dir.display().to_string(),
            source,
        })?;
        match crate::endpoint::read_socket(&dir) {
            Ok(Some(address)) => match Self::connect_local(&address, &token) {
                Ok(client) => return Ok(client),
                // A stale `socket` file: the WebSocket URL may still be live.
                Err(err) => {
                    tracing::debug!(target: "regia_agent", "local transport unavailable, using WebSocket: {err}")
                }
            },
            Ok(None) => {}
            // A malformed advertisement must not choose where the token goes: no fallback.
            Err(err) => {
                return Err(ClientError::Connection(format!(
                    "{}: {err}",
                    dir.join(crate::endpoint::SOCKET_FILE).display()
                )));
            }
        }
        Self::connect(&endpoint.url, &token)
    }

    /// Connects a launched editor tool (ADR-1523, `book/agents/editor-tools.md`) to the scoped
    /// listener with the endpoint and token its launcher put in the environment. Returns the
    /// client and the tool's manifest id.
    ///
    /// # Errors
    ///
    /// [`ClientError::Connection`] when the variables are missing (the process was not launched
    /// as a tool) or the connection fails.
    pub fn connect_tool_from_env() -> Result<(Self, String), ClientError> {
        connect_tool(|name| std::env::var(name).ok())
    }

    /// Calls `method` and returns its result.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(Value::from(id)),
            method: method.into(),
            params,
        };
        let text = serde_json::to_string(&request).map_err(|e| ClientError::Protocol(e.to_string()))?;
        match &mut self.transport {
            Transport::WebSocket(socket) => {
                socket
                    .send(Message::Text(text.into()))
                    .map_err(|e| ClientError::Connection(e.to_string()))?;
            }
            Transport::Local(stream) => {
                crate::local::write_frame(stream, text.as_bytes())
                    .map_err(|e| ClientError::Connection(e.to_string()))?;
            }
        }
        loop {
            let text = match &mut self.transport {
                Transport::WebSocket(socket) => match socket.read() {
                    // The read timeout fired (WouldBlock on Unix, TimedOut on Windows): the app is
                    // not answering. Return instead of blocking the caller forever; a late answer
                    // is skipped by request id on the next call.
                    Err(tungstenite::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        return Err(ClientError::TimedOut);
                    }
                    Ok(Message::Text(text)) => Some(text.as_str().to_string()),
                    Ok(Message::Close(_)) => {
                        return Err(ClientError::Connection("server closed the connection".into()));
                    }
                    Err(e) => return Err(ClientError::Connection(e.to_string())),
                    _ => None, // ping/pong handled by tungstenite
                },
                Transport::Local(stream) => {
                    match crate::local::read_frame(stream, crate::local::MAX_FRAME_BYTES) {
                        // The read timeout fired, as on the WebSocket path.
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            return Err(ClientError::TimedOut);
                        }
                        Ok(Some(text)) => Some(text),
                        Ok(None) => {
                            return Err(ClientError::Connection("server closed the connection".into()));
                        }
                        Err(e) => return Err(ClientError::Connection(e.to_string())),
                    }
                }
            };
            let Some(text) = text else {
                continue;
            };
            let response: Response =
                serde_json::from_str(&text).map_err(|e| ClientError::Protocol(e.to_string()))?;
            if response.id != id {
                continue;
            }
            if let Some(error) = response.error {
                return Err(ClientError::Rpc(error));
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
    }
}

/// Whether the server's handshake answer accepts the token: exactly one JSON object with
/// `"authenticated": true` (ADR-5878). A close before the answer, a refusal or anything else
/// is a refusal.
fn handshake_accepted(answer: Option<&str>) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        authenticated: bool,
    }
    answer
        .and_then(|text| serde_json::from_str::<Answer>(text).ok())
        .is_some_and(|answer| answer.authenticated)
}

/// Host passed to the TCP resolver after the URL host passed the loopback allow-list.
fn connection_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

/// [`Client::connect_tool_from_env`] with the environment read through `var`.
fn connect_tool(var: impl Fn(&str) -> Option<String>) -> Result<(Client, String), ClientError> {
    use crate::scope::{ENDPOINT_ENV, ID_ENV, TOKEN_ENV};
    let need = |name: &str| {
        var(name).ok_or_else(|| {
            ClientError::Connection(format!(
                "{name} is not set: this process was not launched as an editor tool"
            ))
        })
    };
    let endpoint = need(ENDPOINT_ENV)?;
    let token = need(TOKEN_ENV)?;
    let id = var(ID_ENV).unwrap_or_default();
    Ok((Client::connect(&endpoint, &token)?, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// integ-v049a (ADR-5923): a `Client` is held in ECS resources (the editor's UI inspector
    /// keeps one for a remote attach), which must be `Send + Sync` on every target. The Windows
    /// named-pipe stream was `Send` only, so the editor stopped compiling for Windows. This is a
    /// compile-time check: it fails to build on a target where either bound is missing.
    #[test]
    fn a_client_is_send_and_sync_on_every_target() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Client>();
        send_sync::<LocalStream>();
    }

    #[test]
    fn ipv6_loopback_host_connects_without_brackets() {
        use std::net::{Ipv6Addr, SocketAddr, ToSocketAddrs};

        let request = "ws://[::1]:41234".into_client_request().expect("IPv6 URL");
        let host = request.uri().host().expect("host");
        let port = request.uri().port_u16().expect("port");
        assert_eq!(host, "[::1]");
        let addresses: Vec<_> = (connection_host(host), port)
            .to_socket_addrs()
            .expect("literal address")
            .collect();
        assert_eq!(addresses, vec![SocketAddr::from((Ipv6Addr::LOCALHOST, 41234))]);
        assert_eq!(connection_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(connection_host("localhost"), "localhost");
    }

    /// Outside a launch the helper names the missing variable; with the variables it connects
    /// to the scoped listener and returns the tool id.
    #[test]
    fn a_tool_connects_from_its_environment() {
        let Err(err) = connect_tool(|_| None) else {
            panic!("no environment, no connection");
        };
        assert!(err.to_string().contains("REGIA_TOOL_ENDPOINT"), "{err}");

        let tokens = crate::scope::ToolTokens::new();
        let scope =
            crate::scope::ToolScope::new("fixture", [crate::schema::Permission::Read], vec!["*".into()]);
        let (token, _) = tokens.issue(scope);
        let server = crate::server::start_scoped(tokens, None).expect("scoped server");
        let url = format!("ws://{}", server.addr);
        let env = |name: &str| match name {
            "REGIA_TOOL_ENDPOINT" => Some(url.clone()),
            "REGIA_TOOL_TOKEN" => Some(token.clone()),
            "REGIA_TOOL_ID" => Some("fixture".to_string()),
            _ => None,
        };
        let (_client, id) = connect_tool(env).expect("connects with the scoped token");
        assert_eq!(id, "fixture");
    }

    /// The endpoint reader only accepts `127.0.0.1`; `localhost` and `[::1]` resolve through
    /// the hosts file / resolver and must not receive the token either.
    #[test]
    fn connect_rejects_localhost_and_ipv6_names() {
        for url in ["ws://localhost:9", "ws://[::1]:9"] {
            let Err(err) = Client::connect(url, "t") else {
                panic!("must refuse {url}");
            };
            assert!(err.to_string().contains("loopback"), "{err}");
        }
    }

    /// ADR-5878: only the exact accept answer is an acceptance.
    #[test]
    fn only_the_accept_answer_accepts() {
        assert!(handshake_accepted(Some(crate::local::HANDSHAKE_ACCEPTED)));
        for answer in [
            Some(crate::local::HANDSHAKE_REFUSED),
            None,
            Some(""),
            Some("{}"),
            Some(r#"{"authenticated":"true"}"#),
            Some(r#"{"authenticated":true,"extra":1}"#),
            Some(r#"{"jsonrpc":"2.0","id":1,"result":true}"#),
        ] {
            assert!(!handshake_accepted(answer), "{answer:?}");
        }
    }

    #[test]
    fn connect_rejects_a_non_loopback_host() {
        let Err(err) = Client::connect("ws://203.0.113.5:9", "t") else {
            panic!("must refuse");
        };
        assert!(err.to_string().contains("loopback"), "{err}");
    }
}
