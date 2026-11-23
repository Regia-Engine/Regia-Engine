# Net dedicated server (agents)

The dedicated-server build and host (spec 76, ADR-1663, ADR-1886): a game's simulation with nobody playing on it, run
under a supervisor on a Linux (or Windows) box. This page covers building and packaging, `server.toml`, the host's lifecycle, the operator
console, remote admin, the health endpoint, signals, exit codes and the agent methods. Guide topic
`net-dedicated-server`. Sessions themselves (handshake, travel, bans, reconnect) are on [net session](net-session.md).

The host is `regia_net::server_host::ServerHost`. A game plugs in through `HostGame`: `AppHostGame` wraps a
`regia_app::App` with `NetSessionPlugin` (one `App::update` per tick; match end and idle slowdown come from the
installed `SessionRules`), `SessionOnlyGame` is a session with no world (tools, tests). In a `server` build
`regia::run` does this for the game.

## Building and packaging

A game gets a server build by declaring it (ADR-1886):

```toml
[dependencies]
regia = { path = "...", default-features = false }   # so `--no-default-features` here drops the client crates

[features]
default = ["client"]
client = ["regia/client", "regia/agent", "regia/ui"]
server = ["regia/server"]
```

With `--no-default-features --features server`, `regia_render`, `regia_audio`, `regia_ui`, `regia_vfx`, `regia_2d`,
`regia_telemetry`, `winit`, `wgpu`, `kira`, `cpal`, `gilrs` and `arboard` are not in the graph at all, and `regia::run`
hosts the game: `ServerPlugins`, the game's plugins, then `SimulationPlugins` (scene, physics, animation, framework,
nav, AI, net; a plugin the game added itself wins), with the fixed step pinned to `tick_hz`. Code that names a render
or audio type goes behind `#[cfg(not(feature = "server"))]`; a server build that needs one fails to compile, and the
engine provides no stand-ins. `NetMode` defaults to `DedicatedServer`; `Offline` and `Client` exit with code 2.
`samples/dedicated_server` is the smallest such game. `cargo xtask check-server-build` runs
`cargo check --locked -p dedicated_server --no-default-features --features server` and fails when the server build
does not compile.

```
regia package --server --platform linux|windows [--profile <name>] [--out DIR] [--dry-run]
```

The built-in `server` profile is base `shipping` over the whole project; a profile with `target = "server"` limits it
to its `entry_levels`, and `--profile dev` builds a dev server. It writes:

```
dist/server-linux/<slug>-server/
    <bin>-server(.exe)         the binary
    content/base.fpak          cooked server content
    content/net-manifest.ron   gameplay content manifest
    assets/                    raw gameplay data: levels, prefabs, tables, config
    server.toml  bans.ron  README-server.txt
```

archived as `<slug>-server-linux.tar.gz` or `<slug>-server-windows.zip`.

| Asset kinds | Server |
|---|---|
| `level`, `prefab`, `netprofile`, `data_table`, `data`, `config`, `heightfield`, `navmesh`, `collision` | Kept and hashed. A file or cooked type the kind table does not know is `data` |
| `skeleton`, `clip` | Kept, not hashed, only when an entity with `HitShapes` on `Joints` or a root-motion key reaches them |
| `texture`, `material`, `shader`, `audio`, `font`, `ui`, `vfx`, `video`, `cinematic`, `mesh`, `model`, `lighting`, `sprite` | Removed. A mesh a collider key points at is `collision` instead of `mesh` |

`--dry-run` cooks if needed, lists what is kept and what the filter removed with its kind, and writes no package. The
package fails on any shipping-binary check, a forbidden crate in `cargo tree` or the binary, a removed kind in the
content, or a missing `net-manifest.ron`. Every package writes `net-manifest.ron`, client packages too, and client and
server packages of one tree write the same bytes, whatever their profiles ship: it covers the whole project's hashed
kinds, by source file for cooked assets. `protocol_hash` is 0 in the file: each process stamps its own.

```
regia server [--project DIR] [--level PATH] [--port N] [--clients N] [--agent]
```

Cooks, builds with the `server` feature in the dev profile and runs from the dev cache, with `build/server-dev/` as the
server directory (`server.toml` is written once and kept, `content/net-manifest.ron` every run; `REGIA_SERVER_DIR`
points the process at it). `--clients N` builds the client with `regia/net` and starts N of them without a window on
loopback (`REGIA_CONNECT`, `REGIA_CLIENT_OFFSCREEN`, `REGIA_CLIENT_NAME`, `REGIA_NET_MANIFEST`); they stop with the
server, and a SIGTERM to `regia server` drains it. `--agent` starts the agent protocol server in the server process. `regia_agent` needs the client crates, so a
build with it links them: dev builds, and a profile with `allow_agent = true`, which skips the no-client-crate checks
with a warning.

## `server.toml`

Format page: `docs/formats/server-toml.md`. Sections and keys: `[server]` `name`, `bind`, `port`, `max_players`,
`password`, `tick_hz`, `motd`, `idle_tick_hz`; `[levels]` `rotation`, `start_index`, `match_end_delay_s`; `[net]`
`client_rate_bytes`, `server_rate_bytes`, `send_every_ticks`, `entropy`, `encryption`, `identity_file`; `[admin]`
`token_file`; `[status]` `enabled`, `bind`, `port`; `[logs]` `dir`, `keep_files`; `[persistence]` (see
[net server persistence](net-server-persistence.md)); `[bots]` `min_players`, `max_bots`, `join_delay_ticks`,
`leave_delay_ticks`, `seed`, `reaction_ticks`, `aim_error_radians`, `perception_radius`.

The `[net]` rates the file names replace the game's `NetSettings.replication` rates (from `config/net.ron`) at start
and on every reload (`HostGame::set_net_rates`, ADR-5650); a key the file does not name keeps the `config/net.ron`
value, and a reload that drops a key goes back to it (ADR-7213). The session tells clients the cadence the server
really sends at (`HostGame::net_rates_in_use`), and `server_rate_bytes` caps the total. The default `server.toml`
ships the three keys commented out. `net.state_channel` shows the effective `send_every_ticks`, `client_rate_bytes`
and `server_rate_bytes`.

- **Precedence:** built-in defaults < `server.toml` < environment (`REGIA_SERVER_PORT`, `REGIA_SERVER_MAX_PLAYERS`,
  `REGIA_SERVER_LEVEL`, `REGIA_SERVER_PASSWORD`) < command line (`--config PATH`, `--port N`, `--bind ADDR`,
  `--max-players N`, `--level PATH`, `--name TEXT`, `--log-dir DIR`, `--console`, `--agent`, `--relay ADDR`,
  `--shared-port`, `--no-port-map`, `--bots N`). `--no-port-map` (N-19, ADR-10069) keeps a host with `--relay
  HOST:PORT` from asking the router for a port mapping (`net.host_check` `mapping_error` `port_map_disabled`).
- **`--upgrade-config`** rewrites the config file (`--config PATH`, else `server.toml`) to the current `format_version`,
  keeping comments and settings, prints what it did and exits 0 without starting; an unreadable file or an unknown version
  exits 2 (`server_host::config::upgrade_config_file`, ADR-5650).
- **Validation:** `format_version` 1..=3 (new files are written as 3); `port` 1..=65535 and not `status.port`; `max_players` 1..=256; `tick_hz`
  10..=240; `send_every_ticks` >= 1; at least one rotation level. Each failure prints `field: expected ...`, exit 2.
  A rotation level that is not in the server content exits 4.
- **Unknown keys** warn (one line each) and are ignored.
- **Reload** (SIGHUP, console `reload`, `server.reload_config`) applies `name`, `motd`, `max_players` and `password`
  (future joins), `levels.rotation` (from the next rotation), the `[net]` rates, `net.entropy` (joins after the
  reload) and `bans.ron`; every other changed
  key is logged as needing a restart.

## Host states

| State | What happens | Leaves to |
|---|---|---|
| `starting` | config, log dir, `content/net-manifest.ron`, `bans.ron`, game port | `loading` (or exit 2/3/4) |
| `loading` | the rotation's current level | `running` (or exit 4) |
| `running` | sessions; one fixed step per tick at `tick_hz`; next rotation level `match_end_delay_s` after `SessionRules::match_over` turns true | `draining` |
| `draining` | new joins refused; every client gets `server_shutting_down`; up to the grace (default 5 s) to flush | `stopped` |
| `stopped` | exit 0 | — |

Pacing: one fixed step per tick; a tick longer than its step is an overrun, followed by up to `FixedTime::max_substeps` catch-up steps (default 5).
Overruns, the worst tick and the 99th percentile over the last minute are logged every 60 s and served by `/health`.
While empty, a game that allows it (`SessionRules::allow_idle_slowdown`) ticks at `idle_tick_hz`; a connecting
client restores `tick_hz` before its handshake completes.

## Console

When stdin is a terminal, or with `--console`, the host reads one command per line. Commands are queued and run at
the start of the next tick; output goes to stdout and the log.

| Command | Effect |
|---|---|
| `status` | name, level, tick, players, overruns in the last minute, state; then one line per session player, `  peer.local ping=<ms>ms` |
| `players` | one line per player: `peer.local name address ping=<ms>ms loss=<percent>% state` (`-` while the transport has no estimate yet; ADR-5650) |
| `kick <player-id> [reason]` | disconnect with `kicked` and the reason |
| `ban <player-id> [duration] [reason]` | add to `bans.ron` (platform id if the authenticator gave one, else the address) and kick; duration `90s`, `30m`, `12h`, `7d`, default permanent |
| `unban <entry-id>` | remove a ban entry |
| `bans` | list the active entries |
| `level <path>` | travel to a level now |
| `next` | travel to the next rotation level |
| `say <text>` | broadcast a server chat line (ignored if the game has no chat handler) |
| `reload` | same as SIGHUP |
| `serverkey` | print the public key clients pin |
| `joincode` | print `join code <code> relay <addr>` (no code without `--relay`; see below) |
| `revokecode` | withdraw the join code on the relay: `join code withdrawn` (a later join by it is refused `join_refused`; joined players stay; N-19, ADR-10069), or `no join code (start the server with --relay)` |
| `relay` | with `--relay local`: one line `relay protocol=2 lobbies=N flows=N allocations=N ports_free=F/P hosts=N most_clients=M/CAP forwarded=N forward_dropped=N host_rate_dropped=N permission_dropped=N full_refused=N version_refused=N`; otherwise `no local relay (start the server with --relay local)` |
| `quit [grace-seconds]` | enter draining (default 5 s) |
| `help` | list commands |

A `<player-id>` is `peer.local` (for example `3.0`), as printed by `players`; `kick 3` kicks every player of peer 3.
Unknown commands print `unknown command` with the `help` hint. Plugins add commands through `HostGameHooks::add_command`:
`ServerPersistencePlugin` adds `save`, `persistence` and `restore` ([net server persistence](net-server-persistence.md#operator-console)).

## Remote admin

With a token file (`admin.token_file`), a connected client sends `AdminLogin { token }` (constant-time compare) and
then `AdminCommand { line }`, which runs as a console line; output returns as `AdminOutput { lines }`. Three wrong
tokens from one peer disconnect it with `auth_failed`. A token file its group or other users can access (any of the
`0o077` mode bits set) turns remote admin off on Linux with a warning: `chmod 600` it. Remote admin needs the encrypted transport: it is on for a UDP host unless `[net] encryption = "off"`, and
a login over a plain link (a loopback peer under `auto`) is refused with `remote admin needs an encrypted link`, without
a strike. Connect with `encryption: Required`, or with the server's key pinned, to administer from the same machine. The
dev-build override `ServerHost::allow_unencrypted_admin` lifts both rules.

## Encryption and the server key

`[net] encryption` is `auto` (default: every non-loopback peer), `required` (every peer) or `off`, and `[net] identity_file`
(default `server.key`) holds the server's key. The file is created on first start with mode 0600; a file its group or other
users can access stops the start (exit code 2), because that key is no longer private. Keep it across updates: a new key breaks every
pin. `serverkey` on the console and the `server_key` log field print the public half. With `encryption = "off"`, a password
or admin token on a non-loopback bind stops the start (exit code 2) instead of crossing the wire readable. Both keys need a
restart. See [Net transport encryption](net-crypto.md).

## Health endpoint

With `[status] enabled = true`, plain HTTP/1.1 on `status.bind:status.port` (default `127.0.0.1:7778`; port 0 = any
free port), one request per connection, 1 KiB limit, 2 s read timeout. Read-only.

| Request | Answer |
|---|---|
| `GET /health` while running | `200` `{"ok":true,"state":"running","tick":123456,"players":7,"max_players":16,"level":"levels/arena.level.ron","uptime_s":3600,"tick_ms_p99":4.20,"overruns_last_min":0,"game_port":"0.0.0.0:7777","join_code":null}` (`game_port` and `join_code` are `null` until known) |
| `GET /health` while draining | `503`, same keys, `"state":"draining"` |
| any other path | `404` |
| any other method | `405` |
| a request over 1 KiB | `431` |

## Signals and exit codes

- Linux: SIGTERM and SIGINT enter draining; a second one exits at once with code 0. SIGHUP reloads. The handlers only
  set flags the host reads at tick start.
- Windows: Ctrl+C, Ctrl+Break and console close drain; another event while draining exits at once with code 0, and a
  close waits up to 4.5 s for the host to stop. A logoff is ignored, so a server under a service wrapper survives an
  admin leaving. Reload is the console `reload`.
- Exit codes: `0` normal shutdown; `2` configuration or command-line error; `3` a port could not be bound (game or
  status); `4` content error (pak, manifest or level); `70` crash (a panic or a Linux or Windows native fault; the platform panic hook writes `logs/crash-<timestamp>.txt`, and a zip crash bundle, when the feedback plugin is installed, goes to the feedback folder, not `logs/`).
  Restarting is the supervisor's job (for example a systemd unit with `Restart=on-failure`).
- Logs: one line per join, leave (with reason), kick, ban, travel and reload, a per-minute stats line, and the phase
  markers `startup`, `level_load`, `running`, `draining`, `stopped`.

## Methods

Available when the server runs with the agent (dev builds) or the profile allows it.

| Method | Mode | Params | Result |
|---|---|---|---|
| `server.status` | read | — | the `/health` fields, with `players` as `[{ player, name, addr, state }]` and the count as `player_count` |
| `server.kick` | play | `{ player, reason? }` | `{ kicked: true }` |
| `server.ban` | play | `{ player, duration_s?, reason? }` | `{ entry_id }` |
| `server.unban` | play | `{ entry_id }` | `{ removed: true }` |
| `server.bans` | read | — | `{ entries: [{ id, kind, value, until, reason, added }] }` |
| `server.change_level` | play | `{ level }` | `{ travel_id }` |
| `server.reload_config` | play | — | `{ applied: [key], needs_restart: [key] }` |
| `server.shutdown` | play (needs confirmation, spec 23 § 5) | `{ grace_s?, confirm: true }` | `{ draining: true }` |

`until` and `added` are Unix seconds on the server's session clock (the wall second at session start plus the
monotonic session clock), the clock the join check reads: a `duration_s: 60` ban ends after 60 s of that clock even
if the system clock steps meanwhile (ADR-5973).

```json
{ "method": "server.kick", "params": { "player": "3.0", "reason": "afk" } }
→ { "kicked": true }

{ "method": "server.reload_config" }
→ { "applied": ["server.motd", "bans.ron"], "needs_restart": ["server.port"] }
```

## Errors

Every error is `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `server_host_missing` | The process is not a dedicated-server host. |
| `net_not_server` | The session is not a server. |
| `player_unknown` | No player with that `peer.local` (or peer) id. |
| `player_ambiguous` | On a shared port, a bare id that names a player on both layers; nothing was done. Say `link:<id>` or `session:<id>`. |
| `ban_entry_unknown` | `server.unban` names no entry. |
| `ban_invalid` | `server.ban` built an entry `bans.ron` refuses (not an address or range). Nothing was done. |
| `ban_store_save_failed` | `bans.ron` could not be saved (a locked file, a full disk, or a read-only store: one that was unreadable at start). For `server.ban` the entry was added and the peer was still kicked, but the entry applies only until the next restart, when it is gone; for `server.unban` the removal is memory-only. Free the file (close the editor holding it) and ban again, or edit `bans.ron` and `server.reload_config` (ADR-5580, ADR-5673). |
| `level_not_in_pak` | `server.change_level` names a level that is not in the server's rotation (the console `level` command takes any level in the pak). |
| `config_invalid` | `server.reload_config` read a file that fails validation (nothing applied). |
| `server_draining` | `server.shutdown` or `server.change_level` while the host is draining or stopped. |
| `confirmation_required` | `server.shutdown` without `confirm: true`: stopping affects every player, so the agent asks the owner first. |
| `server_platform_unsupported` | `regia package --server` for Android or macOS: exit code 2, before anything is built. Not an agent error: the CLI prints it. |

## Replication games and join codes

A game whose traffic is replication over a `NetLink` (the `net-game` template, ADR-2440) inserts a
`regia_net::server_host::HostedLink` in its server app (`HostedLink::new::<MyWire>(options)`). The
host then lets the **link** bind the game port, with `net.encryption`, the `net.identity_file` key,
`server.max_players`, `server.password` and `server.tick_hz` from `server.toml`; its own session
runs local. Console, bans file, rotation, reload, drain and `/health` work as for a session game;
`status`, `players`, `kick <peer>`, `serverkey`, the player count and idle slowdown follow the
link's peers, and a drain sends every peer `server_shutdown`. For link peers use `net.peers` and
`net.status` (the link publishes there).

**Kick and ban of link peers (ADR-2665).** With a `HostedLink`, the console `kick` / `ban` / `unban` and
the agent's `server.kick` / `server.ban` / `server.unban` act on the link's peers (ids as `net.peers`
gives them; `7` and `7.0` are the same). A ban adds an `Address` entry for the peer's IP to the
session's ban list, so it is saved in `bans.ron` with every other ban and listed by `server.bans`;
the link refuses handshakes from that address until the entry expires or is removed, and the peer
is removed with `banned`. On every host tick, a link peer whose address matches an active
`Address` entry (an exact IP or a CIDR range, also one edited into `bans.ron` and `reload`ed) is
removed the same way, and a refusal is lifted once no active entry covers the address (an unban, an
entry deleted from the file and `reload`ed, an expired ban). That check runs when the peers or the
list's length change and at most once a second otherwise. Errors are the usual `player_unknown` and
`ban_entry_unknown`.

**One port for both layers (`--shared-port`, ADR-2665).** By default the link owns the game port and
the session runs local (no session clients). With the server binary's `--shared-port` switch the host
binds the game port once and splits it: a connect request carrying the link game's protocol id goes
to the link, any other to the session, and later traffic follows the layer that accepted the address.
Session clients (remote admin, chat, streams) then connect to the same address and key as the game's
clients. It is a command-line switch, not a `server.toml` key; without a
`HostedLink` it is ignored with a warning. The layers number their players on their own, so an id can
name one on each: `kick` and `ban` then answer `player_ambiguous` and do nothing until the id says
which, `link:7` or `session:7.0`. An id on one layer only needs no prefix. The console's `players`
lists both layers, the session's as `session:<id>`; the player count, idle slowdown and drain count
both too. A ban of a link peer also removes the session's peers on that address. A sealed record
follows its session token, so a peer whose NAT moved it keeps its layer, and a handshake reply whose
challenge row was pushed out by a flood of forged requests goes to both layers, where only the layer
that minted its cookie accepts it. Each layer reads at most `MAX_READS_PER_ROUND` datagrams per
receive. Tests: `regia_net/tests/shared_port.rs`.

`--relay local` runs a rendezvous relay inside the host on `127.0.0.1:7800` (`local:0` picks a free
port); `--relay HOST:PORT` registers with a relay elsewhere. The host registers a join ticket (the
game port and the server key) and prints

```text
listening 0.0.0.0:7777 key <server key>
join code ABC123 relay 127.0.0.1:7800
```

The local relay speaks relay protocol 2 (spec 120 § 10.6, ADR-7982): it has 64 allocation ports
(`LOCAL_RELAY_ALLOCATION_PORTS`), one per relayed client, so up to 40 clients (the per-host cap) reach one host through
it, each seen by the host as its own address. The console command `relay` (and `ServerHost::relay_status`) shows its
allocations, free ports and refusal counters: `permission_dropped` counts datagrams from addresses with no permission
on a port (a forger), `full_refused` allocations refused by a cap, `version_refused` requests in a protocol this relay
does not serve. A rising `permission_dropped` with steady play is someone probing ports; a rising `full_refused` is a
host at its cap.

The console command `joincode` repeats the line, and `/health` (so `server.status`) carries
`"game_port"` and `"join_code"` (`null` until known). A client resolves the code on the relay and
connects to the game port with the key pinned (`net_game_client --join ABC123`, or
`regia_net::server_host::resolve_join_code`). `regia server --clients N` does this on its own.

Join and start errors (printed by the CLI and the template's client; not agent errors):

| Code | When |
|---|---|
| `join_code_invalid` | The text is not a join code (six letters and digits). |
| `join_relay_invalid` | `--relay` is neither `local[:PORT]` nor `HOST:PORT` (server exit 2). |
| `join_relay_bind_failed` | The socket toward the relay, or the local relay's port, cannot be bound (server exit 3). |
| `join_refused` | The relay refused (unknown or expired code, full). |
| `join_ticket_invalid` | The lobby's token is not a join ticket of this engine. |
| `join_timeout` | No answer from the relay after 20 lookups. |
| `link_missing` | The app has a `HostedLink` but no `NetLink` (add `NetLinkPlugin`): exit 3. |
| `link_bind_failed` | The game port cannot be bound: exit 3. |
| `link_password_plain` | A password would cross a plain non-loopback link: exit 2. |

A native fault (SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT, SIGTRAP) in a Linux server ends it with
exit code 70, like a panic (`server_host::install_native_crash_exit`, installed by
`regia::run_with`). On Windows it exits 70 too (`TerminateProcess` from the exception filter), so a supervisor matching 70 restarts it.

Tests: `regia_net/tests/server_host_e2e.rs` (a session game found by join code, a link game's
`players`/`kick`/drain, start failures; OS-picked ports, test clock) and the template's
`game/tests/hosted.rs` (two clients join by code, play and leave at fixed ticks, twice, same
transcript).


## Silence policy (ADR-7950)

`server.toml` v4 adds `[net] interrupt_ms = 1500`, `timeout_ms = 20000`, and
`start_grace_ms = 30000`. These settings replace the session's project policy and also reach
its replication game port. They take effect at startup; reload reports changed policy keys as
requiring a restart. `regia upgrade-project --write` migrates v1/v2/v3 while preserving comments
and authored values. Monitor `net.status`, `net.peers`, and `session.state` for `interrupted`;
interruption retains the seat and lifecycle phase until authentic traffic returns or silence
reaches the configured timeout.
