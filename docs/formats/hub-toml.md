# `hub.toml` (format_version 2)

The lobby hub process configuration (NL-06, ADR-8648; version 2 is NL-11, ADR-10397), next to
`regia-lobby-hub`. Read by `regia_net::lobby::hub::HubDocument::from_toml`. `HubFile::from_toml`
reads the same text and keeps the listen, cap, limit and status fields. Every
`regia package --server` writes the commented default (`lobby::hub::DEFAULT_HUB_TOML`).

```toml
format_version = 2
bind = "0.0.0.0"
port = 7810
key_path = ""

[caps]
session_cap = 1024
lobby_cap = 256
max_members = 64
chat_per_sec = 4
handle_max_bytes = 24
idle_ms = 30000

[limits]
per_address_per_sec = 30
per_identity_per_sec = 20

[status]
enabled = true
bind = "127.0.0.1"
port = 7811

[relay]
enabled = true
bind = "0.0.0.0"
port = 7812
allocation_cap = 256
per_address_per_sec = 30

[bans]
path = "hub-bans.txt"

[log]
dir = "logs"
```

Version 1 still loads. A missing `format_version` is version 1. A version-1 file that omits
`relay.enabled` leaves the relay off, including a version-1 `[relay]` table that sets other keys
and omits `enabled`. A version-2 file that omits `relay.enabled` turns it on. An explicit
`enabled` wins. `regia upgrade-project` rewrites version 1 to version 2. The version token may
be any TOML integer spelling of 1 (`1`, `+1`, `0x1`, `0o1`, `0b1`). When `[relay]` is absent
the migrator appends the table with `enabled = false`. When a version-1 `[relay]` table omits
`enabled`, it writes `enabled = false` into that table and keeps the other keys. It appends
`[bans]` and `[log]` when those tables are absent, so an existing hub does not open port 7812
until an operator turns the relay on. A second run changes no byte. A fresh package writes
`enabled = true`. Another version is `format_version: expected 1 or 2, found N` and the process
exits 2.

An empty `key_path` means `$XDG_CONFIG_HOME/regia/lobby-hub.key`, else `$HOME/.config/regia/lobby-hub.key`,
else `%APPDATA%\regia\lobby-hub.key`. The file is created on first run. A path whose ancestor holds
`Regia.toml` is refused: the key is never placed inside a project.

`relay.port` is not the hub listen port. `allocation_cap` is how many allocation sockets the relay
opens (`1..=4096`, default 256). `relay.per_address_per_sec` is the relay's own source limit
(default 30). `0` means no limit. A bad address or a port of 0 is a named field error even when
the relay is off. Binding the relay port is exit 3.

`bans.path` is one player key per line (64 hex digits). A blank line and a line that starts with
`#` are skipped. A missing file is an empty set. More than 4,096 keys is a field error and the
previous set stays. The process logs the count, not the keys. `log.dir` is created at start and
the process appends `hub.log`. An empty path is a field error. A directory that cannot be created
is exit 2. A later append that fails is ignored.

A bad value or an unknown key is a named field error (`field: expected ...`) and the process
exits 2. Fuzz target `net.hub_toml`. Corpus `hub-toml/v1.toml` stays; `hub-toml/v2.toml` is the
fresh default.

Four AskReach keys under `[limits]` are optional (ADR-10392): `ask_per_identity_per_10s`
(default 4), `ask_per_address_per_10s` (default 16), `ask_inflight_per_identity` (default 2) and
`ask_inflight_hub` (default 64). A file that omits them keeps those defaults. The migrator does
not insert them. `HubFile::settings` clamps each into `1..=` that default and does not raise a
lower setting. Zero is "an integer 1 or more".

When `relay.enabled` is set, the same process polls a `RelayServer` on the hub's tick. SIGTERM or
SIGINT stops new relay allocations and keeps refresh of allocations already issued. The process
exits once those allocations are gone, or once 30 seconds of the process clock have passed. A
second signal exits at once. `GET /health` and `lobby.hub_status` report `relay_allocations`,
`servers_online` and `draining`. HTTP 503 while draining.

Key rotation is two processes. The process holds one Noise static key. Bring the new key up on a
new port, ship a game config whose pin list tries the new hex key and then the old one, wait the
adoption window, stop the old process, and drop the old hex key from the next game config. A
client that pins a key this process does not hold ends the link with `hub_key_mismatch` and gets
no session.
