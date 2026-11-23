# Local transport (Unix socket / named pipe)

The agent protocol serves its JSON-RPC 2.0 surface over **two transports at once** (specs/23
§ 2): the loopback WebSocket listener (`ws://127.0.0.1:<random port>`) and a **local
transport** — a Unix domain socket on Linux/macOS, a named pipe on Windows. Both carry exactly
the same messages; the local transport exists for tools that prefer a filesystem path over a
TCP port (shell scripts, editor integrations, a `curl`-style probe). There is no local
transport on Android: dev builds there reach the WebSocket listener through `adb forward`.

## Where it lives

A running app with the agent feature (ADR-014) binds, beside the WebSocket listener:

- **Linux/macOS:** `<project>/.regia/agent/agent.sock` — mode 0600 inside the 0700 agent
  directory, so only the owner can connect.
- **Windows:** the named pipe `\\.\pipe\regia-agent-<pid>-<serial>` (one name per server,
  so two servers in one process never share it), created with an owner-only DACL (the current
  user, `SYSTEM` and `Administrators`, protected from inheritance — the `regia_net`
  private-file scheme, ADR-4828) as the **first instance** of its name: if another process
  already created that name, the start fails and the app keeps only its WebSocket listener
  (ADR-5878).

The address is advertised in `<project>/.regia/agent/socket`, next to `endpoint.json`, in the
same owner-only write. Cleanup only ever removes what is this app's own (ADR-5878):

- At exit the `socket` file is removed only while it names a listener this process still
  serves; another app's advertisement stays.
- Dropping the server removes the Unix socket file only while the path still holds *its*
  socket (same device and inode); a successor's socket is left alone.
- At start, a socket file nothing listens on (a crashed run's: a connect is refused) is
  replaced. A socket another app still serves is **never** unlinked: the start fails with
  `AddrInUse`, that second app runs on its WebSocket URL only, and clients reach it through
  the WebSocket fallback below. A symlink or any non-socket at the path is refused, never
  followed.
- A project path long enough that `.regia/agent/agent.sock` exceeds the platform's socket
  path limit (107 bytes on Linux, 103 on macOS) binds the socket in the account's private
  runtime directory instead (ADR-7335): `$TMPDIR/regia-agent-<key>/agent.sock` on macOS,
  `$XDG_RUNTIME_DIR/regia-agent-<key>/agent.sock` on Linux, where `<key>` is a hash of the agent
  directory's path (the same path on every start of that project). The directory is created
  0700, and only inside a base directory no other account can write into; the `socket` file
  advertises that address, so clients need nothing new. Without such a base directory (Linux
  with no `XDG_RUNTIME_DIR`), or when even that path is too long, the app logs
  `local transport not started` and serves WebSocket only.

## The wire (ADR-5708)

Spec 23 names the transport but not its byte format; the framing is:

1. **Handshake frame:** the client's first frame must be exactly
   `{"bearer":"<token>"}` — the token from the owner-only `token` file, the counterpart of
   the WebSocket handshake's `Authorization` header — at most **256 bytes**, complete within
   **10 s in total** (each read gets only the time left, so a peer trickling bytes cannot hold
   a connection slot).
2. **Handshake answer (ADR-5878):** the server answers one frame,
   `{"authenticated":true}` — JSON-RPC frames follow — or, like a WebSocket 401,
   `{"authenticated":false}` and closes (a wrong token, a malformed, oversized or late
   handshake). The client therefore knows at connect time whether its token was taken.
3. **Frames:** a 4-byte little-endian length, then that many bytes of UTF-8 JSON — one JSON-RPC
   message per frame, the same text a WebSocket message holds. The per-frame cap is 16 MiB,
   as on the WebSocket path; an over-cap frame ends that connection only.
4. **Then the protocol is identical:** requests, notifications (bounded in flight), the
   request's own `timeout_ms` deadline, F0006 on a world that does not take the request in
   time, `render.out_progress` answers from the progress board during a render-out step, and a
   queued `render.out_cancel` or `agent.job_cancel` wakes a render-out waiting on a stalled
   output (ADR-2396).
5. **One shared budget, taken in turns (ADR-7454):** the app answers at most 64 requests a
   frame across both transports. The WebSocket inbox and the local inbox take turns, one
   answered request each, starting with the WebSocket inbox every frame, so a client that keeps
   the other transport's queue full slows a local call by at most a frame and never stops it.
   Requests the connection already answered (F0006, a progress-board poll) cost nothing and
   keep the turn.

## Connecting

`regia_agent::client::Client::connect_project` prefers the local transport when the
`socket` file is present and falls back to the WebSocket URL when the advertised listener is
dead **or refuses the token** (a stale `socket` file of a crashed app, or a live listener of
another app in the same project, does not lock clients out; a *malformed* advertisement is
refused outright — it must not choose where the bearer token goes). On Windows the client
opens the pipe for identification only (the server cannot impersonate it) and checks that the
pipe's server process is the one the name carries before it sends the token, so a pipe name
another account created first never receives it.
`Client::connect_local(address, token)` connects explicitly. `regia agent call` and
`regia mcp` go through `connect_project`, so they use the local transport automatically.

Read timeouts (`Client::set_read_timeout`) work on every local transport: the Unix socket's
receive timeout, and on Windows overlapped pipe reads bounded by a wait and cancelled on
timeout. The server's handshake deadline uses the same mechanism on both. A call the app
does not answer in time returns `ClientError::TimedOut`; the connection stays usable.

## Tests

`crates/regia_agent/tests/protocol_transport.rs` (`spec23_sec2_*`) covers the round trip, the
token refusal at connect, the WebSocket fallback past a refusing listener, the client read
timeout, the owner-only permissions, stale-file, live-socket and symlink handling, cleanup that
keeps a successor's socket and another app's advertisement, the oversized handshake and frame,
and the `connect_project` preference. The round trip, refusal, fallback, timeout and preference
tests also run on Windows against the pipe. `regia_agent::local` unit tests cover the frame
codec, the 256-byte handshake cap and the total handshake deadline (a scripted stream with a
fake clock); `regia_agent::endpoint` and `platform::pipe_rules` unit tests cover the pipe
names, the server-pid check, the connect-error classes of the accept loop, timeouts and
back-off on every host.
