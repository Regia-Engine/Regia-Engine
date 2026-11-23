# Lobby hub process (agents)

The `regia-lobby-hub` process (NL-06, ADR-8648) loads `hub.toml` and serves a [`HubHost`] until
SIGTERM or SIGINT, then drains. It does not add an agent method. `lobby.hub_status` (see
[Lobby hub status](net-lobby-hub.md)) reads the report the process publishes under the label `hub`.

`GET /health` on `status.bind:status.port` returns the same numbers as JSON: `sessions`, `lobbies`,
`listed`, `state_bytes`, `draining`, `refused`. The status is 200 while running and 503 while draining.

## Configuration

`hub.toml` (format `hub-toml` v1, [`docs/formats/hub-toml.md`](../../docs/formats/hub-toml.md)) sets
the listen address, the server key path, the caps and the rate limits. A bad value or an unknown key
prints `field: expected ...` and the process exits 2. A port that cannot be bound exits 3.

The key file is created on first run. An empty `key_path` uses the user config directory. A path
inside a project (an ancestor holds `Regia.toml`) is refused with field `key_path`.

## Rate limits

`limits.per_address_per_sec` (default 30) and `limits.per_identity_per_sec` (default 20) count
requests in a one-second window on the process clock. The gate runs before the hub core, so it also
runs before member-data limits.

| Code | When |
|---|---|
| `hub_address_rate_limited` | Too many requests from one address in one second. |
| `hub_identity_rate_limited` | Too many requests from one player key in one second. |
| `hub_draining` | The hub is shutting down and takes no new session. |

## Package

Every `regia package --server` copies `regia-lobby-hub` (`.exe` for a Windows package) from beside
the `regia` binary, writes `hub.toml`, appends a section to `README-server.txt` and writes
`deploy/regia-hub.service`, whose `ExecStart` is the packaged binary with `--config` and
`--key-file` (ADR-8891). The package does not build the hub; a missing binary stops the package
with a message that names the build command. No relay unit is written: Regia has no standalone
relay binary yet. The `--json` object's `lobby_hub` is true for a server package.

## Verify

`cargo test -p regia_net --test lobby_hub_binary` runs the soak, the named field error, the address
flood and the drain. `cargo test -p regia_net --test lobby_hub_fixes` runs the create-and-list
flood twice: with the address gate open (the core's rewind bound, 1,998 of 2,000 lists) and under
the default gate (at most `per_address_per_sec` of the flooder's requests per second reach the
core). `cargo test -p regia_cli --bin regia server_package` stages a stub binary and checks that
every unit under `deploy/` starts a packaged binary with arguments `regia-lobby-hub` accepts.
