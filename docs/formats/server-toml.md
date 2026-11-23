# `server.toml` (format_version 4)

The dedicated server's configuration (spec 76 § 4, ADR-1663), next to the server binary. Read by
`regia_net::server_host::config::resolve_config`; `regia package --server` writes the commented default
(`server_host::config::DEFAULT_SERVER_TOML`).

```toml
format_version = 4

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
# playlist = "rotation.playlist.ron"  # exclusive with rotation
start_index = 0
match_end_delay_s = 10

[net]
# client_rate_bytes = 49152   # absent = config/net.ron decides (ADR-7213)
# server_rate_bytes = 0
# send_every_ticks = 2
interrupt_ms = 1500
timeout_ms = 20000
start_grace_ms = 30000
mtu_probing = false

[admin]
token_file = "admin.token"

[status]
enabled = false
bind = "127.0.0.1"
port = 7778

[logs]
dir = "logs"
keep_files = 10

[persistence]
enabled = false
dir = "saves"
autosave_ticks = 18000
keep = 5
save_on_shutdown = true
save_on_level_change = true
player_identity = "platform"
```

| Key | Default | Valid | Reload |
|---|---|---|---|
| `server.name` | `"Regia server"` | text | applied |
| `server.bind` | `0.0.0.0` | IP address | restart |
| `server.port` | 7777 | 1..=65535, not `status.port` | restart |
| `server.max_players` | 16 | 1..=256 (players, not peers) | applied (future joins) |
| `server.password` | `""` (none) | text | applied (future joins) |
| `server.tick_hz` | 60 | 10..=240 | restart |
| `server.motd` | `""` | text | applied |
| `server.idle_tick_hz` | 10 | 1..=240 (used only if the game allows idle slowdown) | restart |
| `levels.rotation` | none | map paths; exclusive with `levels.playlist`. The starting row must be in the server content (else exit 4). A later missing row is skipped with `server_next_level_failed` | applied (from the next rotation) |
| `levels.playlist` | `""` | a `.playlist.ron` path inside the server directory; exclusive with `levels.rotation` (ADR-8345) | applied (from the next rotation) |
| `levels.start_index` | 0 | index into the row list | restart |
| `levels.match_end_delay_s` | 10 | seconds >= 0 | restart |
| `net.client_rate_bytes` | absent (`config/net.ron`, default 49152) | bytes/s per client | applied: a named key overrides `config/net.ron`'s `replication.client_rate_bytes`; it is also the session's negotiated cap (ADR-5650, ADR-7213) |
| `net.server_rate_bytes` | absent (`config/net.ron`, default 0 = no cap) | bytes/s | applied: a named key overrides `config/net.ron`'s `replication.server_rate_bytes` (ADR-5650, ADR-7213) |
| `net.send_every_ticks` | absent (`config/net.ron`, default 2) | >= 1 | applied: a named key overrides `config/net.ron`'s `replication.send_every_ticks`; `Welcome` advertises the cadence the server sends at, named or not (ADR-7213) |
| `admin.token_file` | `"admin.token"` | path; missing file = remote admin off | restart |
| `status.enabled` | false | bool | restart |
| `status.bind` | `127.0.0.1` | IP address | restart |
| `status.port` | 7778 | 0..=65535 (0 = any free port) | restart |
| `logs.dir` | `"logs"` | path relative to the server directory | restart |
| `logs.keep_files` | 10 | count | restart |
| `persistence.enabled` | `false` | bool (v2) | restart |
| `persistence.dir` | `"saves"` | a non-empty path inside the server directory | restart |
| `persistence.autosave_ticks` | 18000 | host ticks between autosaves, 0 = none | restart |
| `persistence.keep` | 5 | 1..=1000 saves kept per level | restart |
| `persistence.save_on_shutdown` | `true` | bool | restart |
| `persistence.save_on_level_change` | `true` | bool | restart |
| `persistence.player_identity` | `"platform"` | `"platform"` or `"platform_or_name"` | restart |

**Precedence.** Built-in defaults < `server.toml` < environment (`REGIA_SERVER_PORT`, `REGIA_SERVER_MAX_PLAYERS`,
`REGIA_SERVER_LEVEL`, `REGIA_SERVER_PASSWORD`) < command line (`--config PATH`, `--port N`, `--bind ADDR`,
`--max-players N`, `--level PATH` (replaces the rotation with one level and clears `levels.playlist`), `--name TEXT`, `--log-dir DIR`,
`--console`). A missing default `server.toml` is not an error; a missing `--config` file is.

**Validation.** Every failing field prints one line `field: expected ...` and the host exits with code 2. Unknown keys
and sections print one warning each and are ignored, so newer files run on older servers.

**Versioning.** Version 2 adds `[persistence]` (ADR-3354); version 3 adds `[bots]` and
`net.entropy` (ADR-4854). Version 4 adds the shared silence policy (ADR-7950), the optional `[net] mtu_probing`
(ADR-8300) and the optional `levels.playlist` (ADR-8345), all in one unreleased v4 (ADR-8529, ADR-8553; there is no
version 5). Versions 1, 2 and 3 still load.
`regia upgrade-project` migrates them to v4 by rewriting the version line; comments and every other line remain unchanged.
Unsupported versions are refused. `cargo xtask check-formats` fingerprints every key and the
network enum spellings, with migration sources registered in `regia_net::formats` (`server_toml_migration_sources` is
1, 2 and 3).

## Additive `[bots]` keys (ADR-3346)

These optional keys are recorded in v3. Current readers also accept them in v1/v2 files;
the migrator preserves explicitly authored values:

| Key | Default | Valid | Reload |
|---|---|---|---|
| `bots.min_players` | 0 | 0..=`server.max_players` | restart |
| `bots.max_bots` | 0 (disabled) | 0..=255 | restart |
| `bots.join_delay_ticks` | 30 | non-negative integer | restart |
| `bots.leave_delay_ticks` | 30 | non-negative integer | restart |
| `bots.seed` | 0 | non-negative integer | restart |
| `bots.reaction_ticks` | 12 | non-negative integer | restart |
| `bots.aim_error_radians` | 0.03 | finite 0..=PI | restart |
| `bots.perception_radius` | 40.0 | finite >= 0 | restart |

`min_players` counts connected humans (including reconnect reservations) plus bots. `max_bots` includes
reconnect takeovers. The net_game server's `--bots N` sets `min_players` and `max_bots` to N after file and
environment resolution. An explicitly inserted `NetBotsConfig` takes precedence over these host fields.


`net.entropy` accepts `"off"` or `"range"`; when absent, the project's `config/net.ron` decides.
It is a restart-only setting. `--bots N` is also accepted by the hosted server configuration.

**Presence of the `[net]` rates (ADR-7213).** The three rate keys are read only when the file names them. A key the
file does not name keeps the project's `config/net.ron` value, and a reload that drops a key returns to that value. The
default file written by `regia package --server` therefore carries them commented out, like `entropy`. Earlier builds
(ADR-5650) applied the built-in defaults for unnamed keys, which overwrote `config/net.ron`.


## Version 4: silence policy (ADR-7950)

`[net] interrupt_ms = 1500`, `timeout_ms = 20000`, and `start_grace_ms = 30000`
configure the common policy for the dedicated session and its replication link. Durations are
nonnegative integer milliseconds. Validation refuses `timeout_ms` below 1000, `interrupt_ms`
below 100 or not below `timeout_ms`, and `start_grace_ms` above 600000 (one error per field).
`migrate_server_toml` / `regia upgrade-project --write`
upgrade versions 1, 2 and 3 by changing the version line and preserving settings and comments.
Missing keys receive the new defaults. The current default template writes version 4.

All three silence-policy keys require a server restart; reload reports the changed keys.

## Version 4 addition: path MTU probing (ADR-8300)

`[net] mtu_probing = true` turns on path MTU discovery for the game port's links (spec 120 § 4.3): padded probes of
1,280, 1,400 and 1,452 bytes, confirmed only by an acknowledgment, raise the packet size above the 1,200-byte base, and
a black hole drops it back. Optional, default `false`: the wire is unchanged. It is folded into the unreleased version
4 as an additive key (ADR-7724: a release moves a format at most one version step; v0.5.0 shipped version 3), so a file
without it is a valid version 4 file and versions 1 to 3 migrate as above. A non-boolean value is refused
(`net.mtu_probing: true or false`). The key requires a server restart; reload reports it.

## Version 4 addition: `levels.playlist` (ADR-8345, folded by ADR-8529 and ADR-8553)

`[levels] playlist = "rows.playlist.ron"` names a `.playlist.ron` file inside the server directory
([`playlist-ron.md`](playlist-ron.md)): rows of map, mode and options instead of the map-only `levels.rotation`. The two
keys are exclusive (`levels.playlist: exclusive with levels.rotation`), and one of them is required. The parser records
the path (it is a v4 key, never an unknown-key warning); the host loads the playlist at start and on a reload that
changes it. Optional, default empty: a file without it is a valid version 4 file. It joins the same version 4 as the
silence policy and `mtu_probing` (ADR-7724: v0.5.0 shipped version 3, and a release moves a format at most one
version step), so the format has one v4 hash. The frozen corpus keeps `server-toml/v4.ron` (the silence keys, with
`levels.rotation`) and adds `server-toml/v4-playlist.ron` (the playlist path and `mtu_probing = true`).

## Version 4 addition: `[lockstep]` (ADR-8558)

The dedicated server can host a lockstep match instead of the project's own game: the game's `LockstepHostGame`
(`regia_net::server_host::lockstep_game`, over the ECS-free `regia_net::lockstep::server`). The section is optional and
off by default; it is folded into the unreleased version 4 as an additive section (ADR-7724: v0.5.0 shipped version 3),
so a file without it is a valid version 4 file and versions 1 to 3 migrate by their version line as above.

| Key | Default | Range | Meaning |
|---|---|---|---|
| `enabled` | `false` | boolean | host the lockstep game type |
| `max_seats` | `40` | 1..=4096 | seats the match takes; the next one is refused (`lockstep_seats_full`) |
| `input_delay_ticks` | `4` | 1..=1000 | the input delay the match starts with |
| `observer` | `true` | boolean | the server steps the simulation too and votes in the hash quorum |
| `checkpoint_ticks` | `30` | 0..=3600 | ticks between hash votes (`0`: none) |
| `observer_max_lag_ticks` | `60` | 1..=3600 | an observer further behind leaves the quorum until it is back within half of it |
| `observer_max_backlog_ticks` | `600` | `observer_max_lag_ticks`..=65536 | an observer further behind is detached for the match |

A wrong type or an out-of-range value is refused with the field's name (`lockstep.max_seats: 1..=4096`); an unknown
key in the section warns and is ignored, like any other section's. The whole section requires a server restart; reload
reports `lockstep`.
