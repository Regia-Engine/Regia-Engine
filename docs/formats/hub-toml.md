# `hub.toml` (format_version 1)

The lobby hub process configuration (NL-06, ADR-8648), next to `regia-lobby-hub`. Read by
`regia_net::lobby::hub::HubFile::from_toml`. Every `regia package --server` writes the
commented default (`lobby::hub::DEFAULT_HUB_TOML`).

```toml
format_version = 1
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
```

An empty `key_path` means `$XDG_CONFIG_HOME/regia/lobby-hub.key`, else `$HOME/.config/regia/lobby-hub.key`,
else `%APPDATA%\regia\lobby-hub.key`. The file is created on first run. A path whose ancestor holds
`Regia.toml` is refused: the key is never placed inside a project.

A bad value or an unknown key is a named field error (`field: expected ...`) and the process exits 2.
v1 is the first version, so `regia upgrade-project` leaves it as it is. Fuzz target `net.hub_toml`.
