# 76 — Dedicated server package

Status: proposed (ADR-1599). Plan items: "the headless dedicated-server build target" (v0.3.x (b), `master-plan.md` § 3.4) and "the
dedicated-server package" (v0.5.0, § 3.5). Crates: `regia_cli` (`regia package --server`, `regia server`), `regia` (the `server`
feature), `regia_net::server_host` (runtime host: config, signals, console, health). Builds on specs 70 (modes), 72 (rates) and 77
(sessions, bans, content manifest).

Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: no server target exists. Built and used: `regia_cli` `regia package` (`commands::package`, Linux tar.gz / Windows zip /
Android, build profiles with `entry_levels`/`include`/`exclude`/`features`/`allow_agent`, `ship_check`, `.fpak` paks with a
`content/base.fpak` rule, spec 21), `regia_app::{MinimalPlugins, FixedTime { hz, max_substeps }, FramePacing}`, `regia_crash`,
`regia_framework::GameRules`. New here: the `server` feature, `target = "server"` profiles, `regia package --server`, `regia server`,
`regia_net::server_host`, `server.toml`.

Builders read this file, specs 70, 72 and 77 (netcode batch, ADR-1599), plus `21-tooling-and-packaging.md` (packaging, profiles, shipping checks), `28-log-contract.md`
(log files) and `12-assets.md` § 5 (paks).

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-networking/runtime-networking-overview` (what a dedicated server
is and drops), `runtime-networking/runtime-networking-transport-debugging` (operational behavior of a running server). The package layout,
config format and operator interface are Regia's own design (§ 15); no wording, identifiers, command names or switches carried over.

## 1. Purpose and scope

A dedicated server is the game's simulation with nobody playing on it: no window, no renderer, no audio device, no local input. Studios run
it on rented Linux boxes (and sometimes Windows) under a supervisor. This spec defines:

1. **The build:** a `server` feature that compiles the game without rendering, audio or windowing.
2. **The package:** `regia package --server`, its content subset, layout and checks.
3. **The runtime host:** configuration (`server.toml`), command-line overrides, tick pacing, startup, graceful shutdown and reload on
   signals, an operator console, a health endpoint, logs and exit codes.
4. **The dev loop:** `regia server` to run a server from the project without packaging, with the agent protocol.

Not in scope: hosting providers, orchestration and fleet scaling, container images, store-platform server registration (the Steam
transport and lobbies are D5/D6), matchmaking, Android or macOS servers.

## 2. The `server` build

- The `regia` umbrella crate gains feature `server`. With it, `regia::run` installs `MinimalPlugins` plus the simulation plugins (scene,
  physics, animation pose evaluation, framework, AI, nav, net) and **not** `regia_render`, `regia_audio`, `regia_ui` rendering, the platform
  window or `regia_input` devices. `regia_render` and `regia_audio` must not appear in the server build's dependency graph at all (checked,
  § 11 test 2).
- Gameplay code that touches rendering or audio types must be behind `#[cfg(not(feature = "server"))]`. A server build that needs a render
  or audio type fails to compile, which is the intended signal; the engine provides no stand-ins.

### 2.1 Prerequisite: client-only dependencies become optional

Today a render-free build is impossible: `regia_framework` depends unconditionally on `regia_render`, `regia_audio`, `regia_ui`,
`regia_vfx`, `regia_2d`, `regia_platform` and `regia_input`; `regia_ai` depends on `regia_render`; `regia_platform` depends on `winit`, and
`regia_asset` and `regia_crash` reach `winit` through it. The first builder lane of this spec does this split, behind its own ADR
(a feature-graph change across crates), before anything else:

1. `regia_platform` gains a default feature `window` that owns `winit` and the event loop; without it the crate keeps paths, user dirs,
   timing and Android asset access (Android always enables `window`).
2. `regia_framework` gains a default feature `client` that owns its dependencies on `regia_render`, `regia_audio`, `regia_ui`, `regia_vfx`
   and `regia_2d`, and the modules that use them (HUD, feedback screenshots, audio zones, VFX glue); simulation modules
   (character, rules, player, save, input recording, sequencer model) build without it. Modules that mix both are split, not cfg-riddled.
   `regia_input` stays in the server graph for the action layer those modules read; its default feature `devices` owns the device
   backends (`gilrs`, SpaceMouse) and is off in a server build (ADR-1886 (2)).
3. `regia_ai` moves its debug drawing (the only `regia_render` use) behind a default feature `debug_draw`.
4. `regia_crash` and `regia_asset` depend on `regia_platform` with `default-features = false`.
5. `regia`'s `server` feature enables none of `client`, `window`, `debug_draw`; the default features enable all of them, so existing games
   and the editor are unchanged (host-visible only for crates that already use `default-features = false`; listed in the release notes).

The layer checker (`xtask/src/layers.rs`) gains a check that `regia --no-default-features --features server` builds and that its graph
contains none of `regia_render`, `regia_audio`, `regia_ui`, `regia_vfx`, `winit`, `wgpu`, `kira` (§ 11 test 2).
- The game crate declares the feature: `[features] server = ["regia/server"]`. `regia package --server` builds with
  `--no-default-features --features server` plus the profile's features (§ 3).
- `ProcessKind` defaults to `DedicatedServer` in a server build; `Offline` and `Client` are refused at startup with exit code 2.

## 3. Packaging

```
regia package --server --platform linux|windows [--profile <name>] [--out DIR]
```

- Profiles (`21-tooling-and-packaging.md` § "Build profiles", `Regia.toml` `[profiles.<name>]`) gain an optional `target = "server"` key
  (additive, default absent = client; unknown to older tools, which ignore it). A built-in `server` profile exists: base
  `shipping`, the project's `entry_levels`, and the server content rule below. Custom profiles may name `target = "server"` with their
  own levels.
- **Server content.** The server pak holds only what a server needs: levels, prefabs, `.netprofile.ron`, data tables and every other
  gameplay data asset, collision meshes and heightfields, navmesh data, skeletons and animation clips referenced by entities with
  `HitShapes { pose_source: Joints }` or by root-motion characters, and `config/*.ron`. It excludes textures, materials, shaders, audio,
  fonts, UI layouts, VFX and meshes that have no collision use. The closure walk is the normal one (spec 21) with an asset-kind filter;
  `include`/`exclude` globs of the profile still apply. `--dry-run` prints what is kept and what the kind filter removed.
- **Content manifest.** The package writes `content/net-manifest.ron` (spec 77 § 4.1): the gameplay content hash that clients must match.
  It is computed per logical content pack over the gameplay-hashed asset kinds of spec 77 § 4.1, which are a subset of the server content
  above, so client and server packages built from the same tree write the same file. Mods are not in it (they are added at session start).
- **Layout.**

```
dist/server-<platform>/<Name>-server/
    <name>-server(.exe)          the binary
    content/base.fpak            server content (named base.fpak so spec 21's stray-pak check holds)
    content/net-manifest.ron     gameplay content manifest (spec 77)
    assets/                      raw gameplay data read through assets:// (levels, prefabs, tables, config; ADR-1886 (3))
    server.toml                  default configuration (§ 4), commented
    bans.ron                     empty ban list (spec 77 § 7.3)
    README-server.txt            how to run it: generated from § 4-§ 8
```

  Linux is archived as `<Name>-server-linux.tar.gz`, Windows as `<Name>-server-windows.zip`.
- **Checks** (failures stop the package): the shipping-binary checks of spec 21 (no build paths, no agent server unless the profile allows
  it, glibc floor on Linux); no window, render or audio crate linked (the binary's symbol table and `cargo tree` both checked); the server pak
  contains no excluded kind; `net-manifest.ron` present.
- `regia package --server --platform android` (or `macos`) fails with `server_platform_unsupported` (exit code 2) before building.

## 4. Configuration: `server.toml`

```toml
format_version = 4

[server]
name = "My Game Server"           # shown in server lists and the welcome message
bind = "0.0.0.0"                  # address to listen on
port = 7777                       # UDP game port
max_players = 16                  # players, not peers (split-screen counts per player)
password = ""                     # empty = no password (spec 77 § 5)
tick_hz = 60                      # simulation rate; must match the game build's FixedTime unless the game allows overrides
motd = ""                         # message of the day, sent with the welcome
idle_tick_hz = 10                 # tick rate while empty, if the game allows it (§ 5)

[levels]
rotation = ["levels/arena.level.ron"]  # map-only rows; loops. Exclusive with playlist.
# playlist = "rotation.playlist.ron"   # .playlist.ron rows (map, mode, options). Exclusive with rotation.
start_index = 0
match_end_delay_s = 10

[net]
client_rate_bytes = 49152         # spec 72 § 7, per client
server_rate_bytes = 0             # 0 = no total cap
send_every_ticks = 2

[admin]
token_file = "admin.token"        # remote admin token (§ 7); missing file = remote admin off

[status]
enabled = false                   # HTTP health endpoint (§ 8)
bind = "127.0.0.1"
port = 7778

[logs]
dir = "logs"                      # spec 28 log files; relative to the server directory
keep_files = 10
```

- **`[net]` rates (ADR-7213):** `client_rate_bytes`, `server_rate_bytes` and `send_every_ticks` override `config/net.ron`
  only when the file names them; an unnamed key keeps the project's value (the default file ships them commented out).
- **Precedence:** built-in defaults < `server.toml` < environment variables (`REGIA_SERVER_PORT`, `REGIA_SERVER_MAX_PLAYERS`,
  `REGIA_SERVER_LEVEL`, `REGIA_SERVER_PASSWORD`) < command line (`--config PATH`, `--port N`, `--bind ADDR`, `--max-players N`,
  `--level PATH` (replaces the rotation with one level and clears `levels.playlist`), `--name TEXT`, `--log-dir DIR`, `--console`).
- **Validation** at startup: `format_version` known (1 through 4); `port` 1..=65535 and not equal to the status port; `max_players` 1..=256; `tick_hz`
  in 10..=240; `send_every_ticks >= 1`. `levels.rotation` and `levels.playlist` are exclusive: both set is a config error on
  `levels.playlist`, and neither set is a config error on `levels.rotation`. Any failure prints one line per field with the expected range and exits with code 2. The starting row must be in the server content or the host exits with code 4. A later missing row is skipped at travel (`server_next_level_failed`), not at startup (ADR-8345).
- Unknown keys warn (one line each) and are ignored, so newer config files run on older servers.
- Migration note: `server.toml` is `format_version` 4 (`levels.playlist`, ADR-8345). Version 1, 2 and 3 files still load.
  `regia upgrade-project` (and `--upgrade-config`) rewrites them to version 4 by the version line; comments and every other line stay.
  `--upgrade-config` rewrites the config file in place (atomic, comments kept) and exits 0, or 2 on an error (ADR-5650).

## 5. Runtime host lifecycle

| State | What happens | Leaves to |
|---|---|---|
| Starting | Parse config and overrides; open logs; mount `content/base.fpak`; read `net-manifest.ron`; load bans; bind the game port | Loading (or exit 2/3/4) |
| Loading | Load the rotation's current level | Running (or exit 4 if the level fails) |
| Running | Accept sessions (spec 77); simulate at `tick_hz`; rotate levels when the match ends (below) | Draining |
| Draining | Refuse new joins (`server_shutting_down`); send that reason to every connected client; flush for up to 5 s | Stopped |
| Stopped | Write the log summary (spec 28 § 4); exit | — |

- **Tick pacing.** The host runs exactly one fixed step per tick on the server clock (`FixedTime` at `tick_hz`, `FramePacing` sleeps the rest
  of the tick; no busy-waiting). A tick that takes longer than its step counts as an overrun; up to `max_substeps` catch-up steps follow
  (`FixedTime::max_substeps`, default 5); the overrun count, worst tick time and the 99th percentile over the last minute are logged every 60 s and served
  by the health endpoint.
- **Match end.** The host polls `SessionRules::match_over` once per tick. With the framework `net` feature, `NetSessionBridgePlugin` installs `GameRulesSessionRules` when the app has not called `set_session_rules` (an explicit install still wins). Those rules answer from `GameRules::is_match_over` and, when the world holds `MatchRules`, from `session.board.match_over`. `on_travel_finished` calls `GameRules::on_level_start`. When the poll turns true, the host waits `levels.match_end_delay_s` (default 10 s) on its own clock and travels to the next playlist row (spec 77 § 6.2, ADR-8345). A row that fails to load is logged as `server_next_level_failed`, emitted to the game, and skipped. The host tries each following row once and stops when the next candidate is the current row. `levels.rotation` is map-only (`mode` empty). A `.playlist.ron` row sets `LevelSettings.game_rules` to its mode and copies `options` into `MatchOptions`. `server.rotation_get` and `server.rotation_skip` read and advance that list; without a dedicated host they return `server_no_rotation`. A `.playlist.ron` may shuffle or weight its rows and gate them on the players on the game port; passed rows are listed in `server.rotation_get` `skipped` (ADR-8897).
- **Idle.** With no players, the server still ticks (timers, AI), but `server.idle_tick_hz` (default 10) may lower the rate while empty if
  the game opts in (`SessionRules::allow_idle_slowdown`, spec 77 § 9); the first join restores `tick_hz` before its handshake completes.
- **Signals.** Decision (ADR-1585): Unix signals are registered with the `signal-hook` crate (MIT OR Apache-2.0; safe registration API; pure
  Rust over `libc`, already in the lockfile; needs a docs/03 row and passes `cargo deny check`); Windows console control events use
  `windows-sys` `SetConsoleCtrlHandler` (feature `Win32_System_Console`) inside `regia_platform::console_ctrl`, which already may use
  `unsafe` (docs/04 § 5 hid row; extend that row's text in the ADR). Both only set an atomic request flag that the host reads at tick start.
  Linux: SIGTERM and SIGINT enter Draining; a second one exits at once with code 0 after the log flush. SIGHUP reloads the
  hot-reloadable fields: `name`, `motd`, `max_players` (for future joins), `password` (for future joins), `levels.rotation` (from the next
  rotation), `[net]` rates, `bans.ron`. Windows: Ctrl+C, Ctrl+Break and console close enter Draining; reload is the console command `reload`
  (§ 6). Other fields need a restart; a reload that changes one logs which ones were ignored.
- **Crashes.** `regia_crash` is the opt-in uploader, not the handler. It does not write a bundle, does not write `logs/`, and does not exit. A panic exits 70 because `regia::server::exit_70_on_panic` (installed before plugins) runs the previous hook and then `process::exit(70)`. The platform hook `regia_platform::crash::install_panic_hook` writes `logs/crash-<timestamp>.txt` through `write_crash_report` and then calls that hook. A Linux or Windows native fault exits 70 through `regia_net::server_host::crash::install_native_crash_exit` (`_exit` on Linux, `TerminateProcess` on Windows). macOS and Android servers do not install that native exit. A zip crash bundle, when the feedback plugin is installed, is `regia_framework::feedback::crash::write_crash_bundle` in the feedback folder, not under `logs/`. The uploader sends pending `crash-*.txt` files on a later start, and only with an endpoint and consent. Restarting is the
  supervisor's job; README-server.txt shows a systemd unit with `Restart=on-failure` as an example.

Exit codes: `0` normal shutdown; `2` configuration or command-line error; `3` cannot bind a port; `4` content error (pak, manifest or level);
`70` crash.

## 6. Operator console

When stdin is a terminal, or with `--console`, the host reads one command per line from stdin. Output goes to stdout as plain lines (and to
the log). Commands:

| Command | Effect |
|---|---|
| `status` | Name, level, tick, players and their ping, overruns |
| `players` | One line per player: player id, name, peer address, ping, loss, state |
| `kick <player-id> [reason]` | Disconnect with `kicked` and the reason |
| `ban <player-id> [duration] [reason]` | Add to `bans.ron` (spec 77 § 7.3) and kick; duration like `30m`, `12h`, `7d`, default permanent |
| `unban <entry-id>` | Remove a ban entry |
| `bans` | List ban entries |
| `level <path>` | Travel to a level now (spec 77 § 6.2) |
| `next` | Travel to the next level in the rotation |
| `say <text>` | Broadcast a server chat line (a session broadcast message the game handles; ignored if the game registers none) |
| `reload` | Same as SIGHUP |
| `quit [grace-seconds]` | Enter Draining (default 5 s grace) |
| `help` | List commands |

A `<player-id>` is written `peer.local` (for example `3.0`), as printed by `players`; `kick <peer>` without `.local` kicks every player of
that peer. Unknown commands print `unknown command` and the help hint. Commands never block the tick: they are queued and run at the start of the
next tick.

## 7. Remote admin

- With a non-empty `admin.token_file`, a connected client that sends the session message `AdminLogin { token }` (constant-time compare)
  becomes an admin for that session and may send `AdminCommand { line }`, which runs as a console line; the output returns as
  `AdminOutput { lines }`. Three wrong tokens from one peer disconnect it with `auth_failed`. The token file must be owner-only (mode
  `0600`, no group or other bits) on Linux (the host refuses to start remote admin, with a warning, if it is not).
- Admin traffic rides the encrypted transport (T2.3, ADR-1901). `[net] encryption = "off"` disables remote admin, and a login
  over a plain link is refused per link without a strike (a loopback peer under `auto`); a dev-build override lifts both.

## 8. Health endpoint

- With `[status] enabled = true`, the host serves plain HTTP/1.1 on `status.bind:status.port` (default `127.0.0.1:7778`), with std
  networking only (no new dependency), one request per connection, 1 KiB request limit, 2 s read timeout:
  - `GET /health` → `200` `{"ok":true,"state":"running","tick":123456,"players":7,"max_players":16,"level":"levels/arena.level.ron",
    "uptime_s":3600,"tick_ms_p99":4.2,"overruns_last_min":0}`; during Draining `503` with `"state":"draining"`.
  - Any other path → `404`; any other method → `405`.
- It is read-only by design: no admin through HTTP.

## 9. Logs

- Log files follow `28-log-contract.md` (JSON lines, phase markers, `summary.json` at clean exit) under `logs.dir`, keeping the newest
  `logs.keep_files` runs. Phase markers: `startup`, `level_load`, `running`, `draining`, `stopped`.
- One info line per join, leave (with reason), kick, ban, travel and reload; per-minute stats line (players, tick time p50/p99, overruns,
  bytes in/out). No per-tick info lines.

## 10. Dev loop: `regia server`

```
regia server [--project DIR] [--level PATH] [--port N] [--clients N] [--agent]
```

- Builds the game with the `server` feature in the dev profile and runs it from the project's dev asset cache (no pak). `--clients N` also
  launches N headless clients (the client build, offscreen) that join it on loopback; this is the hook the session harness of spec 41 uses.
- `--agent` starts the agent protocol server in the dedicated server process (dev builds only), so agents can drive it (§ 12).

## 11. Tests and acceptance

1. **Package layout.** `regia package --server --platform linux` on the arena sample produces the § 3 layout and archive; `--dry-run` lists
   kept and filtered assets and writes nothing.
2. **No client crates.** The server build's `cargo tree -e normal --features server` contains no `regia_render`, `regia_audio`, `winit`,
   `wgpu` or `kira`; a test fixture game that uses a render resource under the `server` feature fails to compile (a compile-fail check:
   the test runs `cargo check` on the fixture and expects failure; no new dependency).
3. **Content filter.** The server pak of the arena sample contains no texture, shader, audio or font asset, and contains every level,
   prefab, profile and collision asset the levels reference.
4. **Manifest parity.** Client and server packages built from the same tree write byte-identical `net-manifest.ron`, although the server
   pak lacks presentation assets and holds only some animation clips (none of them are hashed kinds).
5. **Config.** Precedence (default < file < env < CLI) for every overridable key; each validation rule fails with exit code 2 and a
   field-level message; unknown keys warn and continue.
6. **Lifecycle.** In-process host test with the loopback transport: Starting → Loading → Running; two clients join; a SIGTERM equivalent
   (the host's shutdown request, the same path the signal handler calls) sends `server_shutting_down` to both and exits 0; a second request
   exits at once.
7. **Reload.** Changing `motd` and `max_players` then reloading applies them; changing `port` logs that it needs a restart.
8. **Console.** Every § 6 command, driven through the host's console input channel (not a real terminal), with its effect checked;
   commands queue to the next tick.
9. **Remote admin.** Correct token grants admin; three wrong tokens kick with `auth_failed`; a token file with group or other access bits disables remote admin.
10. **Health endpoint.** `GET /health` returns the JSON shape with the running state; `503` while draining; `404`/`405` for others; a 2 KiB
    request is refused. (The test binds port 0 and reads the chosen port; no fixed port.)
11. **Exit codes.** Bad config → 2; port in use (the test binds it first) → 3; missing level → 4.
12. **Pacing.** The host reads time through an injectable clock (`HostClock` trait; the real one wraps the monotonic clock). Over 600 ticks
    with a no-op game and a test clock advanced by exactly one step per tick, the tick counter advances once per step and records zero
    overruns; a test system that advances the test clock by 1.5 steps inside one tick records one overrun. No sleeps, no wall clock.
13. **Unsupported platform.** `--server --platform android` fails before building with `server_platform_unsupported`.

## 12. Agent protocol surface

Available when the server runs with `--agent` (dev builds) or the profile sets `allow_agent = true`. Guide page:
`book/agents/net-dedicated-server.md` (topic `net-dedicated-server`; add its `book/COVERAGE.md` rows): building and packaging, `server.toml`, console and admin, health,
exit codes, the dev loop, and every error code.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `server.status` | read | — | the `/health` JSON plus per-player rows: `players` is the row list and the count is `player_count` (ADR-5650) |
| `server.kick` | play | `{ player, reason? }` | `{ kicked: true }` |
| `server.ban` | play | `{ player, duration_s?, reason? }` | `{ entry_id }` |
| `server.unban` | play | `{ entry_id }` | `{ removed: true }` |
| `server.bans` | read | — | the ban entries |
| `server.change_level` | play | `{ level }` | `{ travel_id }` |
| `server.reload_config` | play | — | `{ applied: [key], needs_restart: [key] }` |
| `server.shutdown` | play (needs confirmation, spec 23 § 5) | `{ grace_s? }` | `{ draining: true }` |

Errors (`F0002` with `data.reason` set to the lowercase code, as neighboring built modules do; Decision (ADR-1585)): `net_not_server`, `player_unknown`, `ban_entry_unknown`, `level_not_in_pak`, `config_invalid`, `server_platform_unsupported`
(packaging), `server_draining`.

## 13. Platform notes

- **Linux x86_64** is the primary server platform (glibc floor of spec 21; aarch64 Linux servers build too but are not gated).
- **Windows x86_64** servers build and run; signals map as in § 5; checked in the Windows pass of each release.
- **Android** and **macOS** are not server platforms (§ 3). Android keeps listen-server support (spec 70 § 11).
- Steam dedicated-server login and registration come with the Steam transport work (D5/D6), not here.
- **As built (w7-net-fuzz-seam-z):** the host's OS-specific code (the native crash exit of § 5, the signal and console
  handlers, the admin token's access check of § 7) lives in `regia_net::server_host::platform/` (`unix.rs`, `linux.rs`,
  `windows.rs`, `other.rs`, `crash_unsupported.rs`) behind three internal functions (`docs/04-conventions.md` § 8); behavior is
  unchanged.

## 14. Performance budget

- Startup to Running for the arena sample under 3 s on the Desktop reference class of `06-testing-and-ci.md` § 3 (counted in log phase markers, not asserted in tests).
- Idle server (0 players, `tick_hz` 60, no idle slowdown): under 2 % of one core on the Desktop reference class; with idle slowdown at 10 Hz,
  under 0.5 %. Measured in the release bench, not in unit tests.
- Memory: the server build of the arena sample under 25 % of the client build's resident memory with the same level loaded.

## 15. Provenance

- Set spec ids compared in review (ADR-1585): `runtime-networking-overview`, `runtime-networking-transport-debugging`; the set only says a dedicated
  server sheds player-oriented work, so the package, config, console and health design have no counterpart there. Set spec ids used: `runtime-networking-overview` (what a dedicated server drops and why), `runtime-networking-transport-debugging`
  (operational behavior of a running server: statistics, logging of connection trouble) (behavior only, ADR-1526).
- Public sources: systemd documentation (`systemd.service`, `Restart=`; public man pages) for the supervisor example; POSIX signal
  semantics (SIGTERM, SIGINT, SIGHUP) from the POSIX standard; RFC 9112 (HTTP/1.1 message syntax) for the minimal health endpoint;
  Windows console control handler semantics from Microsoft's public `SetConsoleCtrlHandler` documentation.
- Regia design choices: the `server` feature boundary and the "no render crate in the graph" rule, the content kind filter, the
  `net-manifest.ron` placement, `server.toml` keys and precedence, the console command set, the exit codes, the idle slowdown option.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 16. Replication games and join codes (ADR-2440)

- A game whose traffic is a `NetLink` (the `net-game` template) inserts `server_host::HostedLink`. The link then binds the game
  port with `net.encryption`, the `net.identity_file` key, `server.max_players`, `server.password` and `server.tick_hz`; the
  host's session runs local. Console, bans file, rotation, reload, drain and `/health` behave as in §§ 5-9; `status`,
  `players`, `kick`, `serverkey`, the player count and idle slowdown follow the link's peers; a drain sends every peer
  `server_shutdown`.
- `--relay local[:PORT]` runs a rendezvous relay inside the host on 127.0.0.1 (default 7800); `--relay HOST:PORT` uses one
  elsewhere. The host registers a join ticket (game port and server key) and prints `join code <code> relay <addr>`; the console
  command `joincode` repeats it; `/health` carries `game_port` and `join_code`. `regia server` passes `--relay local:0` and hands
  the code to its clients (`REGIA_JOIN_CODE`, `REGIA_RELAY`); a project with `server/` and `client/` crates builds each on its own.
- A native fault in a Linux or Windows server exits 70 (§ 5 "Crashes"; ADR-5650 corrects the earlier "on Windows the exception
  code stays the status").
- The net manifest's heightfield hash is platform neutral: portable relative tags, parsed extents as little-endian `f32` bits,
  little-endian `u64` framing.
- In-process acceptance with OS-picked ports and a test clock: `regia_net/tests/server_host_e2e.rs`, the template's
  `game/tests/hosted.rs`.
- Link peers (ADR-2665): `kick`, `ban` and `unban` (console and agent `server.*`) act on the link's peers; a ban is an
  `Address` entry of the session's ban list (`bans.ron`), the link refuses that address, and each host tick removes link peers
  that an active address or range entry matches.
- `--shared-port` (ADR-2665, off by default): the host binds the game port once and serves the link and the session on it
  (`server_host::SharedPort`: connect requests by protocol id, later traffic by the layer that accepted the address, sealed
  records by session token). A command-line switch; `server.toml` v1 is unchanged. An id on both layers is refused with
  `player_ambiguous` until it says `link:<id>` or `session:<id>`.

## 17. World persistence (ADR-3354)

- `server.toml` v2 adds `[persistence]` (`enabled`, `dir`, `autosave_ticks`, `keep`, `save_on_shutdown`,
  `save_on_level_change`, `player_identity`); v1 files load with persistence off and `regia upgrade-project` rewrites them.
- The host raises `HostEvent`s through `HostGame::host_event`: `Started` (after the first level loaded, before any session
  update; an `Err` is exit 4), `Ticked`, `LevelLeaving`, `LevelLoaded`, `Stopping` (after the drain). Unknown console
  commands go to `HostGame::console_command`; an `AppHostGame` reads both from its `HostGameHooks` resource.
- `regia_framework::server_persistence::ServerPersistencePlugin` (added by `regia::run` in a server build) saves through the
  save framework every `autosave_ticks` host ticks, when players leave, before a level change and after a graceful
  shutdown, into `<dir>/<level folder>/world-<sequence>.save` (atomic, rotated to `keep`); it restores the newest save that
  decodes on `Started` and `LevelLoaded`, sets a bad one aside as `*.save.corrupt`, and fails the start with
  `persistence_no_good_save` when none decodes. Per-player components are kept by platform identity (spec 77 § 4).
- Console `save`, `persistence`, `restore <slot> confirm`; agent `server.save_now`, `server.persistence_status`,
  `server.restore`. Details: `book/agents/net-server-persistence.md`.

## 18. Match results (audit gaps GR-10.1 to GR-10.4)

- `regia_framework::match_results::MatchResultsPlugin` (added by `regia::run` in a server build; a listen-server game adds
  it itself) writes one `match-result` v1 record per finished match of the `MatchRules` session to
  `<server dir>/matches/<utc-date>/<match_id>.json` (the root is set on `HostEvent::Started` unless the game set one).
  Writes are atomic and run on the IO task pool; a failed write logs `match_result_write_failed` and is retried once.
- Settings (`MatchResultsSettings`, a resource the game sets; `server.toml` v4 is frozen, so the keys below join
  `server.toml` only with its next version and a migrator):

| Setting | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Build records, run the sinks and write files. |
| `root` | `<server dir>/matches` on a dedicated server, else none | History folder; none writes no file (sinks still run). |
| `keep_days` | `90` | Remove date folders older than this after a write; `0` keeps every age. |
| `keep_count` | `1000` | Keep at most this many records; `0` keeps every count. |
| `match_id` | `Random` | `Random`, `Fixed(id)` (the host sends the id to its peers) or `Derived { seed }` (every lockstep peer derives it). |

- Records hold no IP address and no display name: player ids only, and a `name:` key is hashed (ADR-8880).
- Sinks (`MatchResultSinks`): `LeaderboardSink`, `regia_steam::match_stats::PlatformStatsSink`, and a game's own.
- Agent `rules.match_results`, `rules.match_result`; CLI `regia server results [--since] [--limit] [--dir] [--json]`.
  Details: `book/agents/match-results.md`; format `docs/formats/match-result.md`.
