# PIE with several clients and net stats

Spec 77 § 10.2 and § 10.4. Two tools for testing a networked game without leaving the editor:

- **Play-in-editor with several clients.** One process hosts a listen or dedicated server and joins 0 to 16
  in-memory clients to it over the loopback transport. No socket is opened. With **Separate processes**
  checked, the server and each client run as their own processes instead (see below).
- **The net stats overlay and the bandwidth view.** Per connection: state, round-trip time, jitter, loss in
  and out, bytes and packets per second. Per message kind: bytes and packets in each direction and their share
  of the total.

## What the clients are

Every client is **session-level**: it runs the real client side of the session handshake (Hello, Welcome,
level ready, join snapshot, `in_game`) and can send game messages, behind the same seeded conditioner the server
uses. Use that to test joining, travel, kicks, bans, streams, conditions and bandwidth.

With a game crate open, each client also gets **its own world** (ADR-2623): when play starts with a mode other
than `offline` and at least one client, the play world becomes the replication server and every client gets an
ECS world running the game's client side, joined to the play world over the in-memory transport. What a client
world holds is real replicated state: the entities the server replicates (with the client's `NetNode` tick),
its own predicted character, the other players as interpolated proxies, plus the level the editor has open
(without its `Replicated` entities, which come from the server).

The game decides its side in `register`, which the editor runs once per world: a client world already holds
`NetMode::Client` when `register` runs, the play world holds no `NetMode` (serve). For example:

<!-- ignore-reason: a fragment of a game's registration; the replication and prediction calls are elided -->
```rust,ignore
fn register(app: &mut App) {
    let mode = app.world().get_resource::<NetMode>().copied().unwrap_or(NetMode::ListenServer);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    // replicate, add_net_input, predict, add_predicted_systems: the same on both sides
    if mode == NetMode::Client {
        install_interpolation(app.world_mut(), 60.0, 1.0);
        app.add_plugins(LinkInterpolationPlugin);
    }
}
```

The editor adds `NetPlugin` and `NetLinkPlugin` when the game does not, and hosts and connects the links itself
(do not start a `NetLink` in `register`: the play world then answers `pie_net_link_busy`). Every world takes one
fixed step per editor frame at the game's tick rate: the clients step as many frames as the play world did, so a
paused session holds them. A step of `n` frames runs `n` turns of the play world and then the clients, so it gives
the same world as `n` single steps and a step longer than the links' silence timeout drops nobody.

The Net tab's mode and client count apply when play starts. A session keeps its client worlds, or its lack of them,
until it ends; `editor.pie_net_clients` reports the reason from the start of the session. Each client has its own
in-memory address, as a machine would, so the server's limit per address does not count them together. If a client
world fails to build, the ones already built unload and the play world stays as the game made it.

The session runs on a manual clock. Each editor frame advances it by 16 ms; an agent advances it with
`editor.pie_net_run { action: "step" }`. A run with the same mode, client count, preset and seed reports the
same numbers every time, which is what the tests assert.

## In the editor

The **Net** tab holds the mode (`offline`, `listen`, `dedicated`), the client count, the conditions preset
(`off`, `lan`, `good`, `average`, `bad`, `terrible`) and Start / Stop. With a mode other than `offline`,
pressing Play starts the session and Stop ends it; pausing play pauses the session clock. The tab shows one
row per client and the bandwidth table; "Overlay lines" adds the plain text form below them.

Under them, **Client worlds** has a client picker, a **Focus input on this client** toggle, the picked
client's numbers (entities, replicated, owned, last applied tick, step time) and its view. The view is a
viewport over the client's world with its own camera and renderer; it possesses the client's game camera when
the game spawns one. While the toggle is on and the pointer is over the view, keys go to that client only, as
`KeyboardInput` events (physical names: `KeyW`, `Digit1`, `ArrowUp`, `Space`). A key pressed over the view is
released when the pointer leaves it, the window loses focus or the Net tab stops drawing it, and every held key
is released when the focus moves, so no key stays down in a client nothing can reach. The main viewport keeps showing the play world, which is the server.
With `editor.play { window: true }` the server's play world also shows in the separate play window, whose keys go to the
play world (the server), never to a client; client input still goes through the focus toggle or `editor.pie_net_key`
([The play window](pie-modes.md#the-play-window)).

## Separate processes

With **Separate processes** checked in the Net tab (`separate_processes: true`), play runs the session as real
processes started the way `regia server --clients n` starts them (spec 76 § 10, ADR-2682; the shared code is
`regia_asset_pipeline::dev_server`):

- **dedicated**: a server process and `clients` client processes;
- **listen**: the editor's PIE session hosts on a real UDP port on loopback and `clients` client processes join it.

The processes run the binaries `regia server --clients 1` keeps in `build/server-dev/bin/` (`<game>-server` and
`<game>-client`); run it once to build them. The editor does not build them yet.

When play starts the editor saves the open level to a play copy, `assets/.pie/play-<pid>-<n>.level.ron`, and
writes a server directory `build/pie/<pid>-<n>/` whose `server.toml` binds `127.0.0.1` on an ephemeral game port,
names the play copy as the one level, and serves `/health` on an ephemeral port. Both ports are picked by
binding port 0 and passed on; no port is fixed. The server also gets `--level` and `--port`. Clients start once
the server printed its `listening <addr>` line and dial that address.

Every process gets the same environment on top of what `regia server` passes (the server: `REGIA_SERVER_DIR`,
`REGIA_PROJECT`, `REGIA_ASSETS`; a client: `REGIA_CONNECT`, `REGIA_CLIENT_NAME`, `REGIA_NET_MANIFEST`,
`REGIA_AUDIO=off`):

| Variable | Value |
|---|---|
| `REGIA_PIE_LEVEL` | Absolute path of the play copy. |
| `REGIA_PIE_NET_MODE` | `listen` or `dedicated`. |
| `REGIA_NET_CONDITIONER` | The conditions preset (`off`, `lan`, `good`, `average`, `bad`, `terrible`). |
| `REGIA_NET_CONDITIONER_SEED` | The conditioner seed. |
| `REGIA_CLIENT_OFFSCREEN=1` | Clients, unless **Windowed clients** is checked. |
| `REGIA_CLIENT_TITLE` | Windowed clients: `PIE client k` (the engine's dev client titles its window with it). |

Each client is its own window (windowed) or runs offscreen; the Net tab's client worlds do not run in this mode
(`editor.pie_net_clients` gives the reason `separate_processes`).

Every line a child writes to stdout or stderr goes to the Output Log (target `regia_editor::pie_process`) with a
`[server]` or `[client k]` prefix, and to that child's last 40 lines. Stopping play (or `editor.pie_net_run stop`)
kills every child and removes the play copy and the server directory. The children also end with the editor:
on Linux through the parent-death signal, on Windows through a job object; on macOS only the editor's own
stop and clean exit end them. A child that exits on its own with a failure is **crashed**: the Net tab shows it
in red with its exit code and its last lines, and the other processes keep running.

Peers: in dedicated mode `peers` is the server's `/health` `players`; in listen mode it is the players the
editor's session holds, counted the same way.

## Agent methods

| Method | Mode | Params | Returns |
|---|---|---|---|
| `editor.pie_net_config` | play | `{ mode?, clients?, preset?, seed?, overlay?, separate_processes?, windowed? }` | `{ mode, clients, preset, seed, overlay, running, separate_processes, windowed }` |
| `editor.pie_net_run` | play | `{ action: "start"\|"stop"\|"step", frames?, separate_processes? }` | `{ running, frames, in_game, separate_processes }` |
| `editor.pie_net_state` | read | none | `{ config, frames, in_game, lines, stats?, traffic?, dedicated?, processes_state?, peers? }` |
| `editor.pie_processes` | read | `{ tail? }` | `{ state, mode, windowed, port, status_port, addr, join_code, level, peers, alive, error, separate_processes, processes: [process row] }` |
| `editor.play` | play | `{ separate_processes? }` | `{ queued, hint }` |
| `editor.pie_net_clients` | read | none | `{ running, reason?, message?, tick_hz?, server_frames?, focus, picked?, clients: [client row] }` |
| `editor.pie_net_entities` | read | `{ client, all?, limit? }` | `{ client, last_applied_tick, total, count, entities: [{ entity, net_id, owned, name, translation }] }` |
| `editor.pie_net_focus` | play | `{ client: n \| null }` | `{ focus, picked }` |
| `editor.pie_net_key` | play | `{ key, down? }` | `{ client, key, down }` |
| `editor.capture_viewport` | read | `{ client, width?, height? }` | `{ client, width, height, mime, png_base64, view, possessed, render_us }` |
| `editor.hierarchy` | read | `{ client, root?, max_depth?, filter? }` | `{ roots, count, client }` |
| `net.stats` | read | none | `{ role, now_ms, connections: [row], lines }` |
| `net.traffic` | read | none | `{ kinds: [kind row], total_out, total_in, messages, lines }` |

`net.stats` and `net.traffic` work in any app that runs `NetSessionPlugin`, on a server: they read the loopback
and simulated clients (`net.sim_clients`) of its session. In the editor, `editor.pie_net_state` returns the same
`stats` and `traffic` objects for the PIE session.

A stats **row** is `{ label, peer, state, simulated, rtt_ms, jitter_ms, loss_out, loss_in, bytes_out_per_s,
bytes_in_per_s, packets_per_s, bytes_out, bytes_in, corrections_per_s, interp_delay_ms }`. `out` is client to
server, `in` is server to client. Rates are over the last full second of the session clock. `rtt_ms` and
`jitter_ms` are the smoothed one-way delays of both directions added (the gains of RFC 6298); loss is the
fraction of messages the conditioner dropped (reliable messages are resent, never lost, so loss shows on the
unreliable ones). `corrections_per_s` (spec 73) and `interp_delay_ms` (spec 74) are `null` for session-level
clients: they have no predicted or interpolated entities.

A **kind row** is `{ kind, packets_out, bytes_out, packets_in, bytes_in, share }`. Kinds are the session
messages: `hello`, `welcome`, `refused`, `level_ready`, `join_removed`, `join_records`, `in_game`,
`travel_begin`, `goodbye`, `keepalive`, `player_removed`, `stream_begin`, `stream_chunk`, `admin_login`,
`admin_command`, `admin_output`, `game`. `net.traffic` also lists `messages`: payload bytes per registered
net message type of the replication layer (`bytes_sent`, `bytes_received`), the same counters `net.messages`
reports.

A **client row** of `editor.pie_net_clients` is `{ index, label, connected, peer, entities, replicated, owned,
last_applied_tick, frames, last_update_us, mean_update_us, panic }`: `index` is 0-based (`client-1` is 0),
`replicated` counts entities with a `NetId`, `owned` those the client predicts as their owner,
`last_applied_tick` is the newest server tick the client applied. Step times are wall time, for a look only.
Without worlds, `running` is false and `reason` says why: `not_playing`, `offline`, `no_clients`,
`pie_net_no_game` (no game crate open), `pie_net_link_busy`, `pie_net_host_failed`.

A **process row** of `editor.pie_processes` is `{ role: "server"|"client", index, label, pid, state:
"running"|"exited", exit_code, stopped_by_editor, crashed, log_tail }`; `label` is the Output Log prefix, `log_tail`
the last `tail` lines (default 20, at most 40). The session `state` is `idle`, `starting` (the server runs, the
clients wait for its port), `running`, `stopped` (stopped by the editor, or the dedicated server ended) or `failed`
(`error` says why). `editor.pie_net_run { action: "start", separate_processes: true }` starts the processes without
starting play; `editor.play { separate_processes: true }` sets the option and plays.

`editor.pie_net_entities` is what one client sees: its replicated entities in net id order, with `owned` (the
client predicts it as its owner) and `translation`. Names are not replicated, so this, not the hierarchy, is how to
tell a client's character from another's. `all` adds the world's local entities (no `net_id`); `limit` caps the rows
(default 200) while `total` counts them all. Use it to check that input moved only the focused client's character.

`editor.capture_viewport` and `editor.hierarchy` take `client` to answer for that client's world: the capture
renders the client's view with its own renderer (default 640x360, `width`/`height` 16..4096; the other capture
parameters do not apply), the hierarchy lists the client's entities. Both work in the headless editor, without
panes; the capture needs an adapter (`editor_headless` otherwise).

```json
{ "method": "editor.pie_net_config", "params": { "mode": "listen", "clients": 4, "preset": "average", "seed": 7 } }
{ "method": "editor.pie_net_run", "params": { "action": "start" } }
{ "method": "editor.pie_net_run", "params": { "action": "step", "frames": 300 } }
{ "method": "editor.pie_net_state" }
{ "method": "editor.play" }
{ "method": "editor.pie_net_clients" }
{ "method": "editor.pie_net_focus", "params": { "client": 0 } }
{ "method": "editor.pie_net_key", "params": { "key": "KeyD", "down": true } }
{ "method": "editor.pie_net_entities", "params": { "client": 0 } }
{ "method": "editor.capture_viewport", "params": { "client": 1 } }
{ "method": "editor.hierarchy", "params": { "client": 1, "filter": "Avatar" } }
{ "method": "editor.pie_net_config", "params": { "mode": "dedicated", "clients": 2, "separate_processes": true } }
{ "method": "editor.play" }
{ "method": "editor.pie_processes", "params": { "tail": 10 } }
```

## Errors

Codes are in `error.data.code`.

| Code | Meaning |
|---|---|
| `pie_net_clients_limit` | More than 16 clients. `net.sim_clients` scales further for load tests. |
| `preset_unknown` | The conditions preset name is not one of the six. |
| `pie_net_offline` | `start` while the mode is `offline`. |
| `pie_net_not_running` | `step` with no session. |
| `pie_net_host_failed` | The session could not host or join the clients, or a client world could not start (the message says why). |
| `pie_net_no_client_worlds` | A `client` call, `editor.pie_net_entities`, `editor.pie_net_focus` or `editor.pie_net_key` while no client worlds run (`editor.pie_net_clients` says why). |
| `pie_net_client_unknown` | `client` is not one of the running client worlds. |
| `pie_net_no_focus` | `editor.pie_net_key` while no client has the input focus. |
| `pie_process_no_binaries` | Separate processes: no `<game>-server` / `<game>-client` pair in `build/server-dev/bin/` (run `regia server --clients 1` once). |
| `pie_process_no_project` | Separate processes: no project is open. |
| `pie_process_no_level` | Separate processes: no level is open. |
| `pie_process_offline` | Separate processes started while the mode is `offline`. |
| `pie_process_spawn_failed` | A process could not start, or the listen session could not host. |
| `pie_process_io` | The play copy, the server directory or an ephemeral port could not be prepared. |

Start errors of separate processes started by play (not by an agent call) are not returned anywhere else: read
them in `editor.pie_processes` `error`.

`net.stats` and `net.traffic` fail with `net_not_running` when the app has no `NetSession` (see the net session
page).

## Not built

- Building the server and client binaries from the editor: separate processes run what `regia server` built.
- The conditioner inside separate processes: every process gets the preset and seed (`REGIA_NET_CONDITIONER`,
  `REGIA_NET_CONDITIONER_SEED`), but the engine's UDP session has no conditioner hook yet, so the engine's dev
  server and dev client do not apply it; a game may read the variables itself.
- A listen server *process*: in listen mode the editor hosts, since the engine's `server` build is dedicated only.
- Mouse and gamepad input to a client world (keys only).
- Corrections per second and interpolation delay in the session-level stats rows (the client worlds have the
  predicted and interpolated entities; the stats rows still come from the session layer).
- `seeing.screenshot` per client: use `editor.capture_viewport { client }`.

## Per-client conditioner

Each client stores a conditioner preset name in memory (ADR-3996). The default is the session preset
from `editor.pie_net_config`, so a project that never sets a client preset joins every client the way
it does today. Setting client N does not change client M or the session preset. The name is not
written into the project, and there is no format version change.

The name is applied when that client joins the loopback session. Set it before
`editor.pie_net_run { action: "start" }` (or before Play). A link that is already up keeps the preset
it joined with until the next start. Names are the rows of `SESSION_CONDITIONER_PRESETS`: `off`,
`lan`, `good`, `average`, `bad`, `terrible`.

`editor.pie_net_config` is unchanged. Its unknown-preset code stays `preset_unknown`. Per-client
names use a separate method so a bad client name cannot be confused with a bad session name.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `editor.pie_net_client_conditioner` | play | `{ client, preset? }` | `{ client, preset, explicit, session_preset, presets }` |

`client` is 0-based and must be below the configured client count. Omit `preset` to read. `preset` is
the name that client will join with. `explicit` is false when the client is still following the
session preset. `presets` is one resolved name per configured client, index 0 first. `session_preset`
is the session name (`off` when the session preset has not been set).

```json
{ "method": "editor.pie_net_config", "params": { "mode": "listen", "clients": 2, "preset": "off", "seed": 11 } }
{ "method": "editor.pie_net_client_conditioner", "params": { "client": 1, "preset": "bad" } }
{ "method": "editor.pie_net_run", "params": { "action": "start" } }
{ "method": "editor.pie_net_run", "params": { "action": "step", "frames": 400 } }
{ "method": "editor.pie_net_state" }
```

Client 0 stays on `off` (loss 0). Client 1 joins on `bad` and, after the same step count, reports a
higher round-trip delay than client 0.

| Code | Meaning |
|---|---|
| `pie_client_index` | `client` is past the configured client count. Nothing is changed. |
| `pie_conditioner_unknown` | `preset` is not one of the six names. Nothing is changed. |

Simulate (`editor.play { mode: "simulate" }`) and local split-screen players (`editor.play { players }`) are on
[Play-in-editor modes and editor jobs](pie-modes.md); they apply to the play world, not to the client worlds.

## Viewing a role

ADR-7829. With client worlds running, the play toolbar shows **Viewing: Server · Client 1 · Client 2 …**. The
Outliner and Inspector show the viewed world (the server is the play world), and the play viewport's border and
the toolbar tint use that role's theme color (`accents.play_server`, `accents.play_client_1` to `_4`; see
[Play-mode tint](pie-modes.md#play-mode-tint)). Focusing a client's input (the Net tab's checkbox or
`editor.pie_net_focus { client }`) views that client too. Without client worlds there is one world and no switch.

`editor.pie_view { role?, client? }`: `role` is `server` or `client` (with a 0-based `client`); without `role` it
answers the current view. Answer: `{ role, client, label, color, clients }` (`label` is `Server` or `Client N`,
`color` the tint as `#rrggbb`, `null` with the `play.tint` preference off).

Errors (`data.code`): `pie_net_no_client_worlds` (no client worlds run), `pie_view_role_unknown`,
`pie_net_client_unknown` (a client out of range).

```json
{"jsonrpc":"2.0","id":1,"method":"editor.pie_view","params":{"role":"client","client":1}}
```

Not built yet: log lines are not prefixed with the role that wrote them (the log ring records the event target and
message only; the separate-process children's output keeps its `[server]` / `[client k]` prefixes).

## Game tabs

A process of a separate-process session (the dedicated server or a client) can be shown inside the editor in
its own **Game** dock tab (ADR-7831, parity row P-286). In the Net tab, under the processes table, **Show in a
Game tab** lists the running processes; each button opens (or brings forward) that process's tab, titled
`Game: server` or `Game: client k`. `editor.pie_process_view { process, open: true }` does the same.

The process keeps running offscreen. The tab's worker thread talks to the process's own agent server (the
endpoint it advertises in `build/pie/<session>/agent/<role>/`, as the Profiler's process source does) and:

- asks `play.frame { max_width: 640, max_height: 360 }` about fifteen times a second, one call at a time, and
  shows the newest picture with its **frame number** and its **latency** (request to decoded picture);
- sends the toolbar's commands in order, ahead of the next picture: **Pause** / **Resume** (`play.pause`,
  `play.resume`), **Step** (`play.step { frames: 1 }`: exactly one frame of game time, then paused),
  **Slower** / **Faster** (`play.set_time_scale` through 0.1, 0.25, 0.5, 1, 2, 4 and 8), **Mute**
  (`play.mute`);
- forwards input while the picture has keyboard focus (`play.inject_input`): every key (Tab and the arrows
  included), pointer moves, buttons and the wheel over the picture, with positions in the process's own pixels.

The picture is a focusable widget: click it or reach it with Tab. **Shift+F1** gives the keyboard back to the
editor (the release key of in-process play). The editor frame never waits on the network; a process that is not
up yet or went away shows `connecting` or `disconnected` with the reason, and the worker retries.

Closing a tab stops its stream only: **the process keeps running**. **Stop process** in the toolbar (or
`open: false, stop_process: true`) stops that one process and leaves the others. **Pop out** (clients only)
starts that client again in its own window (`PIE client k`) and closes the tab; the stopped offscreen client
stays in `editor.pie_processes` as stopped by the editor, and the windowed one is a new entry with the same
label. A new play session moves an open tab to the new process of the same role.

### Runtime methods the tabs use

`play.frame { max_width?, max_height? }` (16..=3840, defaults 640 and 360) answers
`{ frame, width, height, source_width, source_height, source, mime, png_bytes, encode_ms, png_base64 }`: the
newest picture scaled down to fit (box filter, aspect kept, never larger than the source) as a fast-compressed
PNG, and the frame number (`Time::frame_count`) it was taken on. `source` is `window` (the renderer's next
present of the primary window) or `feed` (an app without a renderer that fills
`regia_agent::play_view::FrameFeed` itself). With neither it answers `play_frame_no_source`.

`play.mute { muted }` answers `{ before, muted, bus }`: it mutes the game's root audio bus and, on unmute, puts
back the bus's own mute (a bus the game had muted stays muted). `bus` is `null` in an app without audio; the
state is still kept.

`play.get_diagnostics` also answers `game_frames` (frames that ran game time: a step adds exactly one),
`injected_events` and `injected_keys` (raw events `play.inject_input` wrote), so a caller in another process can
check that a step or a key arrived.

### `editor.pie_process_view`

| Param | Meaning |
|---|---|
| `process` | `"server"` or `"client <k>"` (required). |
| `open` | `true` opens or focuses the tab, `false` closes it; absent reads it. |
| `stop_process` | With `open: false`, also stop the process (default `false`). |
| `action` | `pause`, `resume`, `step`, `speed`, `mute`, `unmute`, `input` or `pop_out`. |
| `frames` | For `step`: 1..=10000, default 1. |
| `speed` | For `speed`: the time scale, 0..=1000. |
| `events` | For `input`: raw events as `play.inject_input` takes them, at most 256. |

The answer is the tab's state: `{ process, open, process_running, link, message, frame, width, height,
latency_ms, frames_received, frame_error, commands_done, last_reply, last_error, paused, speed, muted,
input_focus, forwarded }`; for a process with no open tab `{ process, open: false, process_running }`.
Commands are queued and answered in order; `commands_done` counts the answers and `last_reply` /
`last_error` hold the last one, so a caller waits for `commands_done` to grow instead of sleeping.

```json
{ "method": "editor.pie_net_config", "params": { "mode": "dedicated", "clients": 2, "separate_processes": true } }
{ "method": "editor.pie_net_run", "params": { "action": "start" } }
{ "method": "editor.pie_process_view", "params": { "process": "client 0", "open": true } }
{ "method": "editor.pie_process_view", "params": { "process": "client 0", "action": "input",
  "events": [{ "type": "key", "key": "KeyW", "state": "pressed" }] } }
{ "method": "editor.pie_process_view", "params": { "process": "client 0", "action": "pause" } }
{ "method": "editor.pie_process_view", "params": { "process": "client 0", "action": "step" } }
{ "method": "editor.pie_process_view", "params": { "process": "client 0", "open": false } }
```

| Code | Meaning |
|---|---|
| `pie_view_bad_process` | `process` is not `server` or `client <k>`. |
| `pie_view_no_session` | No separate-process session runs. |
| `pie_view_no_process` | The session has no running process of that role (or it exited). |
| `pie_view_not_open` | An `action` for a process with no open tab. |
| `pie_view_bad_action` | `action` is not one of the eight names. |
| `pie_view_no_window` | `pop_out` for the server (it has no window). |
| `pie_view_worker_failed` | The tab's thread could not start. |
| `pie_process_spawn_failed` | `pop_out` could not start the windowed client. |
| `play_frame_no_source` | (`play.frame`, in the process) no renderer and no `FrameFeed` picture. |

### Not built

- Gamepad forwarding: `play.inject_input` has no raw gamepad event; a tab forwards keys and the mouse only.
- The picture of a process with a renderer runs through the screenshot read-back (one capture per request); a
  shared-memory ring was measured against it and not needed at the tab's rate (ADR-7831).
- Sound in the tab: the dev clients still start with `REGIA_AUDIO=off` (a popped-out one too), so **Mute**
  matters for a game that turns its audio on anyway; the tab does not play the process's sound.
