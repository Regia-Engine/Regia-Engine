# Dedicated servers, lobbies and matchmaking

A dedicated server is your game's simulation running with nobody playing on it: no window, no
renderer, no audio. Clients connect over UDP and the server holds the authoritative world. This
chapter takes you from a networked game to a server you can package, run, operate and test, and then
to the lobby and matchmaking layer that gets players onto it. It assumes your game already replicates
state; if not, start with [Networking](networking.md). Method-level detail is on the agent pages
[Net dedicated server](../agents/net-dedicated-server.md), [Net lobbies](../agents/net-lobbies.md),
[Net session](../agents/net-session.md) and [PIE with several clients](../agents/pie-net.md).

## 1. Give your game a server build

A game declares a server build in its own `Cargo.toml`. The `client` feature carries every crate a
window needs, so turning it off removes them from the build:

```toml
[dependencies]
regia = { path = "...", default-features = false }

[features]
default = ["client"]
client = ["regia/client", "regia/agent", "regia/ui"]
server = ["regia/server"]
```

With `--no-default-features --features server`, the renderer, audio, UI, VFX, 2D, telemetry, window
and gamepad crates are not in the dependency graph at all, and `regia::run` hosts the game: the
server plugins, your plugins, then the simulation plugins (scene, physics, animation, framework,
navigation, AI, net) with the fixed step pinned to `tick_hz`. Code that names a render or audio type
goes behind `#[cfg(not(feature = "server"))]`; the engine provides no stand-ins, so a server build
that needs one fails to compile, which is the point. The network mode defaults to
`DedicatedServer`; `Offline` and `Client` exit with code 2. `samples/dedicated_server` is the smallest
such game.

For a quick local loop, `regia server` cooks, builds the server in the dev profile and runs it with
`build/server-dev/` as the server directory:

```text
regia server --project . --level levels/arena.level.ron --port 7777 --clients 2 --agent
```

`--clients N` also starts N windowless clients on loopback and stops them with the server; `--agent`
starts the agent protocol in the server process (dev builds only).

## 2. Package a server

```text
regia package --server --platform linux
regia package --server --platform windows --out dist
regia package --server --dry-run
```

Linux and Windows only; Android and macOS exit with code 2 before anything is built. The default
profile is the built-in `server` (the shipping profile over the whole project). A profile in
`Regia.toml` with `target = "server"` limits the package to its `entry_levels`, and `--profile dev`
builds a dev server. The result is an archive (`<slug>-server-linux.tar.gz` or
`<slug>-server-windows.zip`) holding:

```text
<slug>-server/
    <bin>-server(.exe)         the binary
    content/base.fpak          cooked server content
    content/net-manifest.ron   gameplay content manifest
    assets/                    raw gameplay data: levels, prefabs, tables, config
    server.toml  bans.ron  README-server.txt
```

The package keeps only what a simulation needs. Levels, prefabs, net profiles, data tables, config,
height fields, nav meshes and collision stay. Textures, materials, shaders, audio, fonts, UI, VFX,
video, cinematics, meshes, models, lighting and sprites are removed (a mesh a collider points at stays as
collision). Skeletons and clips stay only when an entity with hit shapes on joints or a root-motion
key reaches them. Run `--dry-run` first: it lists what is kept and what the filter removed, and
writes no package. The package fails on any shipping-binary check, a forbidden crate in the binary, a
removed kind found in the content, or a missing `net-manifest.ron`.

## 3. Configure it: `server.toml`

`server.toml` sits next to the binary (format: `docs/formats/server-toml.md`). The packaged default
is commented; the keys are:

```toml
format_version = 1

[server]
name = "My Game Server"
bind = "0.0.0.0"
port = 7777
max_players = 16
password = ""
tick_hz = 60
motd = ""
idle_tick_hz = 10

[levels]
rotation = ["levels/arena.level.ron"]
start_index = 0
match_end_delay_s = 10

[net]
# client_rate_bytes = 49152   # absent = config/net.ron decides
# server_rate_bytes = 0
# send_every_ticks = 2

[admin]
token_file = "admin.token"

[status]
enabled = false
bind = "127.0.0.1"
port = 7778

[logs]
dir = "logs"
keep_files = 10
```

The three `[net]` rates override the project's `config/net.ron` only when you name them; a key you leave out (or
comment out, as the default file does) keeps the project's value, and clients are told the cadence the server really
sends at.

- **Validation.** `format_version` must be 1; `port` 1 to 65535 and not the status port;
  `max_players` 1 to 256; `tick_hz` 10 to 240; `send_every_ticks` at least 1; at least one rotation
  level. Each failure prints `field: expected ...` and the server exits with code 2. A rotation level
  that is not in the server content exits with code 4. Unknown keys warn once each and are ignored.
- **Precedence.** Built-in defaults, then `server.toml`, then the environment (`REGIA_SERVER_PORT`,
  `REGIA_SERVER_MAX_PLAYERS`, `REGIA_SERVER_LEVEL`, `REGIA_SERVER_PASSWORD`), then the command line
  (`--config PATH`, `--port N`, `--bind ADDR`, `--max-players N`, `--level PATH`, `--name TEXT`,
  `--log-dir DIR`, `--console`).
- **Reload without a restart.** SIGHUP, the console `reload`, or the agent's `server.reload_config`
  applies `name`, `motd`, `max_players` and `password` (for future joins), the level rotation (from
  the next rotation), the `[net]` rates and `bans.ron`. Any other changed key is logged as needing a
  restart; the agent reply lists them under `needs_restart`.
- **Idle slowdown.** While empty, a game that allows it ticks at `idle_tick_hz`, and a connecting
  client restores `tick_hz` before its handshake completes.
- **Encryption.** `[net] encryption` is `auto` (default: every non-loopback peer), `required` or `off`,
  and `[net] identity_file` (default `server.key`) holds the server's key. The key is created on first
  start with mode 0600; a key readable by others stops the start (exit 2). Keep it across updates: a
  new key breaks every pin. With `encryption = "off"`, a password or admin token on a non-loopback
  bind stops the start rather than cross the wire readable.

### Lifecycle and exit codes

The host goes `starting`, `loading`, `running`, `draining`, `stopped`. A level change happens
`match_end_delay_s` after the game reports the match over. Draining refuses new joins, tells every
client `server_shutting_down` and waits up to a grace period (5 seconds by default) to flush.

| Exit code | Meaning |
|---|---|
| 0 | Normal shutdown |
| 2 | Configuration or command-line error |
| 3 | A port could not be bound (game or status) |
| 4 | Content error: pak, manifest or level |
| 70 | Crash; the crash handler writes its bundle into `logs/` |

On Linux, SIGTERM and SIGINT drain (a second one exits at once with code 0). Restarting is the
supervisor's job, for example a systemd unit with `Restart=on-failure`. On Windows, Ctrl+C, Ctrl+Break and
console close drain, and a logoff is ignored so a service wrapper survives an admin leaving.

### Health endpoint

With `[status] enabled = true` the server answers plain HTTP on `status.bind:status.port` (default
`127.0.0.1:7778`), read-only. `GET /health` returns `200` while running with
`{"ok":true,"state":"running","tick":123456,"players":7,"max_players":16,"level":"...","uptime_s":3600,
"tick_ms_p99":4.20,"overruns_last_min":0}`, `503` while draining, `404` for other paths, `405` for
other methods. Point your monitoring at it, and watch `overruns_last_min`: a tick longer than its step
is an overrun, and a sustained rate means `tick_hz` is too high for the machine.

## 4. Operate it: the console

When stdin is a terminal, or with `--console`, the server reads one command per line. Commands run at
the start of the next tick.

| Command | Effect |
|---|---|
| `status` | name, level, tick, players, overruns in the last minute, state |
| `players` | one line per player: id, name, address, ping, loss, state |
| `kick <player-id> [reason]` | disconnect with `kicked` and the reason |
| `ban <player-id> [duration] [reason]` | add to `bans.ron` and kick; duration `90s`, `30m`, `12h`, `7d`, default permanent |
| `unban <entry-id>` | remove a ban entry |
| `bans` | list the active entries |
| `level <path>` | travel to a level now |
| `next` | travel to the next rotation level |
| `say <text>` | broadcast a server chat line |
| `reload` | same as SIGHUP |
| `serverkey` | print the public key clients pin |
| `joincode` | repeat the join code line (when a relay is in use) |
| `quit [grace-seconds]` | start draining |
| `help` | list commands |

A player id is `peer.local`, such as `3.0`, as printed by `players`. `kick 3` kicks every player of
peer 3.

### Remote admin

Set `admin.token_file` to a file holding a secret token. A connected client sends `AdminLogin` with the
token and then `AdminCommand` lines that run like console lines. Three wrong tokens disconnect the peer.
On Linux a token file that group or others can read turns remote admin off with a warning:
`chmod 600` it. Remote admin needs an encrypted link, so connect with encryption required or with the
server key pinned.

### Bans

Bans live in `bans.ron` (format: `docs/formats/bans-ron.md`) and are edited by the console, the
agent or by hand followed by `reload`:

```ron
BanList(
    format_version: 1,
    entries: [
        (id: 1, kind: PlatformId, value: "steam:76561190000000000", until: None, reason: "cheating", added: 1793000000),
        (id: 2, kind: Address, value: "198.51.100.0/24", until: Some(1793600000), reason: "spam", added: 1793000000),
    ],
)
```

An entry is by platform id, by address or CIDR range, or by name (the weakest). `until` is Unix
seconds or `None` for permanent; an expired entry never matches. The reason is sent to the banned player
with the refusal. Every change rewrites the file atomically, and a file that cannot be read at start
leaves an empty in-memory list and is never overwritten until a reload reads it. A ban added while a
player is in the reconnect grace refuses their resume.

### From a script

With the agent protocol on (dev builds, or a profile with `allow_agent = true`), the same operations are
methods: `server.status`, `server.kick`, `server.ban`, `server.unban`, `server.bans`,
`server.change_level`, `server.reload_config` and `server.shutdown`. Shutdown needs `confirm: true`,
because it affects every player. Errors are `F0002` with a `data.reason` such as `player_unknown`,
`ban_entry_unknown`, `level_not_in_pak`, `config_invalid`, `server_draining` and
`confirmation_required`; the full list is on the
[agent page](../agents/net-dedicated-server.md).

## 5. Join codes and a relay

A server can print a short code players type instead of an address. Start it with `--relay local` (a
relay inside the host on `127.0.0.1:7800`) or `--relay HOST:PORT` (register with a relay elsewhere). The
server prints:

```text
listening 0.0.0.0:7777 key <server key>
join code ABC123 relay 127.0.0.1:7800
```

A client resolves the code on the relay and connects with the server key pinned. A code is six letters
and digits. Start errors print a code: `join_code_invalid`, `join_relay_invalid`,
`join_relay_bind_failed`, `join_refused` (unknown, expired or full), `join_timeout`.

The server registers from its game port, so the address the relay hands out is the game's and a server
behind a NAT is still reachable: a player first tries the server's address and, if nothing answers within
1.5 s, plays through the relay on a port of the relay reserved for that player alone (relay protocol 2,
one allocation per client). The server renews its registration every 15 s, which keeps its router's NAT
mapping toward the relay open. A relay that is too old to carry several players (protocol 1) ends such a
join with `relay_too_old`; `relay_full` means a relay cap was reached (40 players per server by default).
With `--relay local` the console command `relay` and the agent method `net.relay_status` show the
allocations, caps, refusals by reason and the bytes of each allocation; with a remote relay
`net.relay_status` shows the join code and how many players came through the relay.

### What a relay costs to run

Each relayed byte is received once and sent once. A 32-player match at Direct Order 27's state-sync budget
(about 230 kbit/s down and 40 kbit/s up per player) moves about 7.4 Mbit/s from the server to the players
and 1.3 Mbit/s from the players to the server through the relay: about 8.6 Mbit/s in and 8.6 Mbit/s out,
or about 3.9 GB in and 3.9 GB out per match hour. A small virtual server with a 1 Gbit/s port and a
multi-terabyte monthly allowance carries dozens of such matches at once (spec 120 § 10.6). Players whose
direct path works use none of it. A relay needs one UDP port per relayed player (a pool of 4,096 by
default, `RelayConfig::allocation_ports`) and a file-descriptor limit above that.

A replication game built on the net-game template inserts a `HostedLink` in its server app. The host
then lets that link own the game port, with `server.toml` supplying the rates, password and key, and
console `kick` and `ban` act on the link's peers (a ban of one peer bans its IP). With the server binary's
`--shared-port` switch the host splits one port between the link and the session. A bare id can then name
a player on each layer, so `kick` answers `player_ambiguous` until you write `link:7` or `session:7.0`.

## 6. Lobbies

A lobby is a small room held by a platform where players gather before the game: key/value data, per
member data, a member limit, a visibility (`public`, `friends_only`, `private`, `invisible`) and a
joinable flag. Your game drives one `Lobbies` resource, pumped once per frame by `LobbyPlugin`. There are
three backends, shown in `lobby.status` as `backend`:

| Backend | Use |
|---|---|
| `local` | An in-process directory: tests, editor play with several clients, builds without a platform. Deterministic. |
| `steam` | Steam matchmaking, once Steam is ready (`SteamLobbyPlugin`). |
| `none` | No platform: every call answers `lobby_unavailable` (Android has no lobby platform today). |

Lobby calls that wait on a platform (`lobby.create`, `lobby.join`, `lobby.list`) answer with a `request`
id at once; the result arrives on a later frame as an event in `lobby.status` `recent_events`. Wait on
`lobby.status` rather than assuming it is done.

Lobbies the engine creates carry engine keys under `regia.`, which your game cannot set (`reserved_key`).
Listing and matchmaking filter on the same game, protocol hash and (unless `lobby.match_content` is false)
content hash, so a different build never shows up in your list.

Your own data goes in with `lobby.set_data { key, value }` (only the owner), up to 64 keys, keys up to 255
bytes and values up to 8192 bytes. Players mark themselves with `lobby.ready { ready: true }`. Chat is
`lobby.chat { text }` (1 to 4096 bytes).

### Settings

The `lobby` section of `config/net.ron`:

| Field | Default | Meaning |
|---|---|---|
| `lobby.max_members` | 8 | Member limit of lobbies the engine creates (1 to 250) |
| `lobby.visibility` | `Public` | Visibility of lobbies the engine creates |
| `lobby.min_players` | 2 | Members needed before the owner can launch |
| `lobby.match_content` | true | Also require the same content hash |
| `lobby.auto_connect` | true | Members connect the session when the owner launches |
| `lobby.joinable_in_game` | false | Keep the lobby joinable after launch |
| `lobby.rounds` | Close, Default, Far, Worldwide | Matchmaking rounds |
| `lobby.results_per_round` | 50 | Results per round (1 to 50) |
| `lobby.step_timeout` | 600 | Frames to wait for a platform answer |
| `lobby.create_if_none` | true | Create a lobby when matchmaking finds none |

### Matchmaking

`matchmaking.start` lists lobbies in rounds that widen the distance, joins the first candidate (a full one
moves on to the next), and creates a lobby when every round is empty. Filters are `strings` and `numbers`
comparisons (`eq`, `ne`, `lt`, `le`, `gt`, `ge`), `near` sorting and `open_slots`. Poll
`matchmaking.status`; its `state` is `idle`, `listing`, `joining`, `creating`, `in_lobby`, `launched` or
`failed` (with `no_match`, `create_failed`, `request_timeout`, `left` or `unavailable`). Waits count
frames, never wall time, so a fixed-step run repeats its choices.

When `ready_to_launch` is true (at least `min_players` are in and all are ready) the owner calls
`lobby.launch`. Without an `address` it hosts a listen session on the lobby transport and publishes it;
with an `address` it publishes a server hosted elsewhere, such as your dedicated server
(`udp:<ip:port>`). The lobby closes to new joins, every member with `auto_connect` connects, and the
state becomes `in_game` when play starts. For a dedicated server, wait on `session.state` rather than
the lobby state.

A complete two-process test:

```text
A: matchmaking.start { strings: [{ key: "mode", value: "duel" }], data: { mode: "duel" } }
A: wait_until matchmaking.status state == "in_lobby"
B: matchmaking.start { strings: [{ key: "mode", value: "duel" }] }
B: wait_until matchmaking.status state == "in_lobby"
A: lobby.ready { ready: true }   B: lobby.ready { ready: true }
A: wait_until matchmaking.status ready_to_launch == true
A: lobby.launch { level: "levels/arena.level.ron" }
B: wait_until session.state state == "in_game"
```

## 7. Test under bad networks: the conditioner

A good server survives a bad link. The network conditioner delays, drops, duplicates and reorders traffic
by seeded rules, so the same seed and traffic give the same result and a test of play under 8 percent loss
is repeatable. It is a development tool.

| Preset | Latency | Jitter | Loss | Duplicate | Reorder |
|---|---|---|---|---|---|
| `off` | 0 | 0 | 0 | 0 | 0 |
| `lan` | 1 ms | 0 | 0 | 0 | 0 |
| `good` | 30 ms | 2 ms | 0.1 % | 0 | 0 |
| `average` | 60 ms | 8 ms | 1 % | 0 | 0 |
| `bad` | 120 ms | 20 ms | 3 % | 0 | 1 % |
| `terrible` | 250 ms | 50 ms | 8 % | 2 % | 3 % |

Effects apply in order: loss, delay with jitter, reorder, duplication, then a bandwidth cap. Each direction
has `latency_ms`, `jitter_ms`, `loss_pct`, `duplicate_pct`, `reorder_pct`, `reorder_extra_ms` and
`bandwidth_bytes_per_s` (0 means no cap). Two method families reach it: `net.conditioner_get` and
`net.conditioner_apply { preset?, outbound?, inbound?, seed? }` shape loopback and simulated clients, and
`net.conditioner_set` changes the outbound conditioner of a real endpoint. `net.sim_clients` adds in-process
clients to a server (`idle`, `random` or `recording` inputs, at most 256) that join like real ones, which is
how you load-test a server without a farm of machines.

## 8. Test with several clients in the editor (PIE)

The editor's **Net** tab starts a session in the play world with 0 to 16 clients over an in-memory
transport, so no socket opens. Pick a mode (`offline`, `listen`, `dedicated`), the client count and a
conditions preset (`off`, `lan`, `good`, `average`, `bad`, `terrible`) and press Play. Each client runs the
real handshake. With a game crate open, each also gets its own world running the game's client side,
joined to the play world, so you see real replicated state, prediction and interpolation. Choose a
client in **Client worlds** and tick **Focus input on this client** to send keys to that client only.

The session runs on a manual clock: each editor frame advances it 16 ms, so a run with the same mode,
client count, preset and seed reports the same numbers every time. From a script, drive it with
`editor.pie_net_config`, `editor.pie_net_run { action: "start" | "stop" | "step", frames? }`,
`editor.pie_net_state` and `editor.pie_net_clients`. The Net tab also shows per-connection round-trip time,
jitter, loss and bytes per second, and a bandwidth table per message kind. If a client world fails to
build, the play world stays as the game made it.

The game decides its side in `register`: a client world already holds `NetMode::Client` when `register`
runs, and the play world holds none. Do not start a `NetLink` in `register`; the editor hosts and connects
the links itself, and a second one answers `pie_net_link_busy`.

## Checklist before you ship a server

1. `regia package --server --dry-run` removes only what a simulation does not need.
2. `server.toml` has your real name, port, rotation and a password if needed; the key file is kept
   across updates.
3. Health endpoint on, and monitored.
4. The token file for remote admin is mode 600.
5. A play-through with the `bad` and `terrible` presets, and with `net.sim_clients` at your player cap.
6. A kick, a ban, a level change and a reload tried once while players are connected.
