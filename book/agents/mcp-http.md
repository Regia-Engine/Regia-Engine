# MCP over HTTP

`regia mcp --http` serves the same MCP surface as `regia mcp` over the MCP **Streamable HTTP**
transport instead of stdio (specs/23 § 2, ADR-2346): the same tools (one per protocol method,
generated from `agent.schema`), the same `regia://` resources, the same errors. Use it when the MCP
client cannot spawn a process, or several clients share one app.

```sh
regia mcp --project . --http                   # 127.0.0.1 on a free port
regia mcp --project . --http 127.0.0.1:8931    # a fixed port
regia mcp token --project . --url              # http://127.0.0.1:8931/mcp
regia mcp token --project .                    # the bearer token
```

The server advertises itself in `.regia/agent/mcp_http.json`
(`{ "url": "http://127.0.0.1:8931/mcp", "token_file": ".../mcp_http_token", "pid": 4242, "loopback": true }`).
Read it through `regia mcp token`, not by hand: a project folder is not trusted (a cloned or shared one can hold a
planted `mcp_http.json` naming any file as the token and any host as the URL, and a client that sends the one to
the other leaks the file). `regia mcp token` checks it the way the WebSocket client checks `endpoint.json`: the URL
must be `http://127.0.0.1:<port>/mcp` (or `[::1]`, `localhost`), and the token file must be the one the server
mints, `.regia/agent/mcp_http_token` of this project, a regular file (not a link), owner-only on Unix. Anything else
is refused and nothing is printed. A server started with `--token-file` uses your own file, which the helper
refuses: read that file directly.

Every request carries the token: `Authorization: Bearer <contents of token_file>`. A token is
minted per launch into `.regia/agent/mcp_http_token` (mode 0600 on Unix, an owner-only DACL on Windows, ADR-5489); `--token-file FILE`
uses an owner-provided token instead (at least 16 printable ASCII characters; on Unix the file must
not be readable by group or others, else the command exits and asks for `chmod 600`).

## A session by hand

```sh
URL=$(regia mcp token --project . --url) && TOKEN=$(regia mcp token --project .)
curl -si "$URL" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}'
# -> 200, header Mcp-Session-Id: <id>, body: the initialize result
curl -s "$URL" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -H "Mcp-Session-Id: <id>" \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'          # -> 202
curl -s "$URL" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -H "Mcp-Session-Id: <id>" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'                   # -> 200, the tool list
curl -s -X DELETE "$URL" -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: <id>"   # ends the session
```

- One JSON-RPC message per `POST` (no batches). A request is answered with its response as
  `application/json`; a notification or response with `202 Accepted`.
- `initialize` opens a session and answers `Mcp-Session-Id`; every later message names it. At most
  16 sessions and 64 admitted connections at once (a connection is admitted when a request on it passes the
  token check). A failed `initialize` opens no session. When the table is
  full, a new `initialize` ends sessions unused for 10 minutes, then the least recently used session
  with no request in flight; a client that comes back to an ended session gets `404
  session_not_found` and initializes again.
- A connection has 30 s to send each whole request (head and body), and may idle 30 s between
  keep-alive requests; then it is closed.
- Before its first request is admitted a connection is *pre-auth*: at most 128 of them, in a pool of their own
  (when it is full, the oldest is closed for the newcomer), with 10 s to send that first head. A head may take at
  most 64 reads (`400 head_too_fragmented`). So sockets held open without the token, even from the network under
  `--allow-remote`, cannot use up the admitted slots or keep the token holder out (ADR-3285).
- There is no server-to-client event stream: `GET` answers `405`, which the transport allows.

## Safety

| Rule | Default | Refusal |
|---|---|---|
| Bind address | `127.0.0.1`; a non-loopback `ADDR` needs `--allow-remote` (then put TLS in front) | the command exits |
| Bearer token | always required, compared in constant time | `401 unauthorized` |
| `Origin` (DNS-rebinding protection) | absent, or a loopback origin (`http://localhost:*`, `http://127.0.0.1:*`, `http://[::1]:*`), or an `--allow-origin` entry | `403 origin_forbidden` |
| `Host` on a loopback bind | must name `localhost`, `127.0.0.1` or `[::1]` | `403 host_forbidden` |
| Request head | at most 16 KiB and 64 headers, in at most 64 reads | `431 head_too_large`, `400 head_too_fragmented` |
| Connections | 128 pre-auth (oldest closed when full; 10 s for the first head), 64 admitted | `503 too_many_connections` (admitted slots only) |
| Advertisement | `regia mcp token` reads only a loopback URL and the minted token file | the command exits, printing nothing |
| Body | one `Content-Length` (no chunked bodies), at most `--http-max-body` (default 4 MiB) | `411 length_required`, `413 body_too_large` |
| Body type | `Content-Type: application/json`; `Accept` (when sent) must allow JSON | `415 not_json`, `406 not_acceptable` |

Refusals answer a JSON body `{ "error": { "reason", "message" } }` and close the connection. The
other reasons: `not_found` (a path other than `/mcp`), `no_event_stream` and `method_not_allowed`
(`405`), `bad_request` and `bad_version` (a malformed head), `batch_unsupported`, `not_jsonrpc`,
`session_required`, `session_on_initialize`, `session_not_found` (`404`: initialize again),
`id_in_use` (`409`), `too_many_sessions` and `too_many_connections` (`503`), `session_broken`
(`500`) and `timeout` (`504`, no answer within 300 s).

The request judging lives in `regia_agent::mcp_http` and is fuzzed as `agent.mcp_http_request`
(see [Fuzzing](fuzzing.md)).
