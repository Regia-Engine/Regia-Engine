# Host a game

<!-- index: Listen server -->
<!-- index: Dedicated server -->
<!-- index: Raw UDP -->
<!-- index: Steam sockets -->
<!-- index: Lobby -->
<!-- index: Lobby hub -->
<!-- index: Relay -->
<!-- index: Join code -->
<!-- index: NAT -->
<!-- index: UPnP -->
<!-- index: Content handshake -->
<!-- index: Network conditioner -->
<!-- index: QUIC -->
<!-- index: Voice chat -->
<!-- index: Password -->
<!-- index: Ban -->
<!-- index: Reconnect -->
<!-- index: Headless server | see: Dedicated server -->
<!-- index: Hole punching | see: NAT -->

A procedure for putting players in one session. The reference chapters stay
[Networking](../../user-guide/networking.md),
[Dedicated servers, lobbies and matchmaking](../../user-guide/dedicated-server.md),
and [Steam](../../user-guide/steam.md). The worked replication game is the
`net-game` template, also the sample `samples/net_game`
([Build a networked game](../../tutorials/networked-game.md)).

## What works in this version

| Path | Status |
|---|---|
| Client, listen server, dedicated server | Built. A listen server hosts and has local players. A dedicated server hosts with no local players, no window, no renderer, and no audio. |
| Raw UDP | The session transport for a shipped game. |
| Loopback socket | Tests and editor play. No socket opens. |
| Join codes, relay, NAT traversal, PCP / NAT-PMP / UPnP | Built. Many home routers refuse port mapping. The relay is the fallback. |
| Lobby hub and the in-process lobby directory | Built. They do not need a platform account. |
| Content handshake, passwords, bans, reconnect, host migration, voice, the network conditioner | Built. Limits are in the sections below. |
| Steam sockets and Steam lobbies | In the tree. The 2026-11-05 versions note (ADR-2828) recorded mock tests. A two-account session is an owner procedure ([Steam real run](../../user-guide/steam-real-run.md)). Record a PASS in `docs/specs/24-steam.md` § 4 before treating a real match as done. |
| Steam upload, input manifest, and controller layout | The CLI is in the tree. Credentials come from the environment. The owner creates the Steam app. |
| QUIC | Partial. `regia_net::quic` exists. A shipped session uses raw UDP (or the Steam socket path above). This guide does not treat QUIC as a session transport. |

Android has no lobby platform: lobby calls answer `lobby_unavailable` and
`lobby.status` reports backend `none`. A dedicated-server package is Linux or
Windows. Android and macOS exit with code 2 before a server package is built.
macOS remains a testing build (ADR-009, ADR-264).

## 1. Create a replication game

```text
regia new "Tag Yard" --template net-game
cd tag_yard
```

`net-game` and `net_game` name the same template. The server crate is headless.
The client crate is the window. Build them in separate cargo invocations. Do
not pass `--workspace`.

`regia package --platform linux` does not pack this three-crate layout yet. It
looks for a root package or a `launcher/` crate and stops. Build the client
with cargo until that layout is packaged. A dedicated server of a project
`regia package` understands is `regia package --server` (step 4).

## 2. One machine, dedicated server and a bot

This is a dedicated server plus a client in one process. It is not a listen
server: nobody is playing on the host with a window.

```text
cargo run -p tag_yard_client -- --headless --host --bot --ticks 120
```

Finished when the process prints `client 0 connected=true sealed=true` and
exits 0. The same match over an in-memory network is the template test
`the_in_memory_minute_replays_bit_for_bit`. The encrypted UDP minute is
`a_scripted_minute_over_encrypted_udp_agrees`.

## 3. Two processes on raw UDP

In one terminal:

```text
cargo run -p tag_yard_server --release -- --level levels/arena.level.ron --relay local
```

The server prints a line of this shape:

```text
listening 0.0.0.0:7777 key <server key>
join code ABC123 relay 127.0.0.1:7800
```

In another terminal, one client per player:

```text
cargo run -p tag_yard_client -- --join ABC123
```

`--join` looks the code up on the relay (default `127.0.0.1:7800`) and connects
with the server key pinned. A direct address is `--connect 127.0.0.1:7777
--key <server key>`. Without a pin, the client is protected from a listener on
the path and is not protected from an attacker in the middle.

A project with one root package, rather than this split template, can use the
dev loop. It cooks, builds the server, runs a relay inside the server, and can
start offscreen clients:

```text
regia server --project . --level levels/arena.level.ron --port 7777 --clients 2 --agent
```

`--clients` accepts 0 to 64. `--agent` starts the agent protocol in the server
process. Dev builds accept it. A shipping server does not, unless the profile
sets `allow_agent = true`.

Finished when the server lists the client (`players` on the server console, or
`net.peers` / `net.status` when the agent is on) and the client stays connected
and sealed.

Join failures from the template client and the CLI:

| Code | Meaning |
|---|---|
| `join_code_invalid` | The text is not six letters and digits. |
| `join_relay_invalid` | `--relay` is neither `local` nor `HOST:PORT`. Server exit 2. |
| `join_relay_bind_failed` | The relay socket cannot be bound. Server exit 3. |
| `join_refused` | Unknown, expired, or full. |
| `join_timeout` | The relay did not answer. |
| `relay_too_old` | The relay is protocol 1 and the direct path failed. |
| `relay_full` | A relay cap was reached (40 players per server by default). |

The agent method `session.connect` does not take a join code. Passing one
returns `join_code_unsupported` (`F0002`). Resolve the code (the template's
`--join`, or `regia_net::server_host::resolve_join_code`) and connect by
address.

## 4. Package and operate a dedicated server

```text
regia package --server --platform linux
regia package --server --platform windows --out dist
regia package --server --dry-run
```

`--dry-run` lists what the server filter keeps and removes, and writes no
archive. The archive contains the server binary, cooked gameplay content,
`server.toml`, `bans.ron`, and `README-server.txt`. It also stages
`regia-lobby-hub` and `hub.toml`. It does not stage a standalone relay binary.
A relay runs inside the server (`--relay local` or `--relay HOST:PORT`) or as
your own `RelayServer`.

`server.toml` (format `docs/formats/server-toml.md`) sets the name, bind, port,
`max_players`, password, tick rate, level rotation, and net rates. An empty
password is no password. A password on a non-loopback bind requires encryption.
`session.host` or `session.connect` with a password and `encryption` off
returns `password_needs_encryption`.

Bans live in `bans.ron`. The console, `server.ban`, and a hand edit plus
`reload` all write that file. An entry is a platform id, an address or CIDR, or
a name. `until` is Unix seconds, or absent for a ban that does not expire. A
ban added during reconnect grace refuses the resume.

Operator methods, dev builds or `allow_agent = true`: `server.status`,
`server.kick`, `server.ban`, `server.unban`, `server.bans`,
`server.change_level`, `server.reload_config`, `server.shutdown`. Shutdown
needs `confirm: true` or the call returns `confirmation_required`. Other
refusals use `F0002` and `data.reason` (`player_unknown`, `ban_entry_unknown`,
`level_not_in_pak`, `config_invalid`, `server_draining`). The full list is on
[Net dedicated server (agents)](../../agents/net-dedicated-server.md).

Health, remote admin, and exit codes (0, 2 configuration, 3 port, 4 content,
70 crash) are on that page. A remote-admin token file on Linux must be mode
600 or remote admin stays off. Remote admin requires an encrypted link.

## 5. Listen server

`NetMode::ListenServer` hosts and has local players. From a game that installed
`NetSessionPlugin`:

```json
{ "method": "session.host", "params": { "mode": "listen", "port": 0, "level": "levels/arena.level.ron" } }
```

`port: 0` binds a free port. The result is `{ session_id, port }`. Another
process connects:

```json
{ "method": "session.connect", "params": { "address": "127.0.0.1:40211" } }
```

Step the app (`play.step` or frames), then read `session.state`. Finished when
the client reports `in_game` and the server's `peers` list includes that peer.
`dedicated` on the server report is false for a listen host.

`mode` other than `listen` or `dedicated` is `F0002` with `data.hint` and no
`data.reason`. `already_in_session` means a session is already running.
`net_not_running` means `NetSessionPlugin` is absent.

The editor **Net** tab runs the same choice in the play world: `offline`,
`listen`, or `dedicated`, with 0 to 16 clients on an in-memory transport. See
[Dedicated servers](../../user-guide/dedicated-server.md#8-test-with-several-clients-in-the-editor-pie).

## 6. Finding a host

Players reach a host by address, by join code, or through a lobby.

**Address.** `session.connect` `address`, or the template `--connect`. An
address join code (`mint_address_join`) carries an IP, a port, a nonce, and an
expiry. It is not a secret. The host password is the access control. The host
cannot revoke an address code. Change the password or the port. A relay join
code can be revoked (`revokecode` on the dedicated server).

**Relay and NAT.** `--relay local` listens on `127.0.0.1:7800`. `--relay
HOST:PORT` registers with a relay you run. The server renews every 15 seconds,
which keeps a home NAT mapping toward the relay open. A client tries the
host's address and, when nothing completes, plays through a relay allocation
(one port on the relay per player). On relay protocol 3 the join also exchanges
candidates (the socket's own addresses and any mapped port) and connectivity
checks. Two players behind NATs that map a new port per destination use the
relay. Port mapping asks the gateway with PCP, then NAT-PMP, then UPnP IGD.
`--no-port-map` skips that. `port_map_unavailable` means the platform hid the
routing table or there is no IPv4 default route. `port_map_disabled` means the
flag was set. Detail:
[Finding each other](../../user-guide/networking.md#finding-each-other).

**Lobbies.** One `Lobbies` resource, three backends:

| `lobby.status` backend | When |
|---|---|
| `local` | Tests, editor play, LAN. No platform. |
| `steam` | After `SteamLobbyPlugin` and a running Steam client. Mock-tested. See the Steam section below. |
| `none` | No platform, including Android. Calls return `lobby_unavailable`. |

Engine-created lobbies store the game, protocol hash, and content hash. Lists
and matchmaking filter on them, and on the content hash unless
`lobby.match_content` is false in `config/net.ron`. Players call
`lobby.ready`. The owner calls `lobby.launch` when `ready_to_launch` is true.
Without an address, launch hosts a listen session on the lobby transport.
With an address such as `udp:<ip:port>`, launch publishes a dedicated server
hosted elsewhere. Members with `lobby.auto_connect` then connect. Wait on
`session.state`, not on the lobby, for a dedicated server. The two-process
script is on
[Dedicated servers](../../user-guide/dedicated-server.md#6-lobbies).

**Lobby hub.** `regia package --server` includes `regia-lobby-hub` and
`hub.toml`. Run it and pin its key. Players use `HubLobbyBackend`. Addresses
stay inside the lobby: a listing never contains one. Invites name a player
key, not a display handle. The link is encrypted, including on loopback.
`connect` refuses a missing key (`hub_key_unpinned`). A hub that answers under
another key ends with `hub_key_mismatch`. Read `lobby.hub_status`. The API is
[Self-hosting a lobby hub](../../user-guide/networking.md#self-hosting-a-lobby-hub).

## 7. Content handshake

A joining client must match the server's protocol version, protocol hash, and
gameplay content manifest. A mismatch ends the join:

| Reason | What differed |
|---|---|
| `version_mismatch` | Protocol version. |
| `protocol_mismatch` | `GAME_ID`, protocol hash, or `content_hash`. |
| `content_mismatch` | Gameplay content or mods. The detail names the mod, for example `mod:speedrun: different`. |
| `build_mismatch` | `ServerConfig::build` is set and the builds differ. |

`session.manifest` shows this process's manifest. Bump `PROTOCOL_VERSION` when
a message change is incompatible. Register replicated components in the same
order on every side. The registration order is part of the protocol hash.

## 8. Voice

Voice is a separate plugin pair. `VoiceChatPlugin` (`regia_audio`) on the
client, `NetVoicePlugin` (`regia_net`, feature `voice`) on the server.
`voice_chat.relay` needs the server plugin. The other `voice_chat.*` methods
need the client plugin and return `UNSUPPORTED` (`F0013`) without it.

Consent is reported, never set, by the agent protocol. A person changes it in
user settings. `voice_chat.test_source` and a synthetic loopback do not open a
microphone. A microphone loopback or `voice_chat.mic` without consent returns
`consent_required`.

Built: packet version 1, PCM, Opus when the `voice-opus` feature is on,
jitter buffer, server relay, mute, block, deafen, spatial playback on the
`Voice` bus, echo cancellation, light noise suppression, and automatic gain.
Light is what runs when strong noise suppression is requested. HRTF for chat
is not in this version. There is no voice editor panel. Hardware checks of a
real microphone are outside this page. Method tables and the rest of the
reason codes are on [Voice chat (agents)](../../agents/voice-chat.md).

## 9. Bad networks, reconnect, migration

The conditioner is a development tool. Presets are `off`, `lan`, `good`,
`average`, `bad`, and `terrible`. Apply one with `net.conditioner_apply` in a
dev build (`conditioner_unavailable` in a shipping build). The same seed and
the same traffic repeat. Play a match under `bad` and `terrible`, and with
`net.sim_clients` at your player cap, before you ship a server.
`sim_clients_limit` is 256. `sim_clients_unavailable` is a shipping build.

A dropped client reconnects with its ticket for `reconnect_grace_s` (lockstep
seats have their own resume window). The link reports `Reconnecting`, then
`PeerRestored`, or `ReconnectFailed`. Turn it off with `ReconnectPolicy::off()`.
A ban during the grace period refuses the resume.

When a listen host leaves, a warm successor can take the match. Lockstep
migration is a different path, because every seat already holds the world.
Both are in [Networking](../../user-guide/networking.md) under host migration,
and the lockstep steps are in
[Lockstep and rollback](lockstep-and-rollback.md).

## 10. Steam sockets, lobbies, and upload

Enable the `steam` feature and add `SteamPlugin`, then `SteamLobbyPlugin`.
Without the feature, or on Android, the plugin is a no-op
(`SteamState::Unavailable`). The Steamworks SDK is not in this repository. Set
`STEAM_SDK_LOCATION` at build time. The owner creates the Steam app. Lanes and
this manual never log in to partner tools.

`lobby.launch` on the Steam backend hosts on a `SteamSocket`. Members connect
to the owner's Steam id. It is the same `regia_net` session as raw UDP.
Reliability stays in `regia_net`. Tests use `MockSteam` and `MockSteamNet`.
That is the coverage recorded on 2026-11-05. Run
[Steam real run](../../user-guide/steam-real-run.md) on two machines, with two
accounts, and keep a PASS verdict before you describe a real match as done.
A skipped or `NOT RUN` line is not a pass.

Stage and upload, after the owner has created the app:

```text
regia package --platform linux --steam
regia steam input-manifest
regia steam layout --family deck --check
regia steam upload --branch beta --dry-run
```

`input-manifest` writes `dist/steam/game_actions_<appid>.vdf` from
`config/input.ron`. `--stdout` prints it. `--out` picks a path and needs no
app id. `layout --family` is `xbox`, `playstation`, `switch`, or `deck`.
`--check` exits 1 when an action has no gamepad binding, unless
`--allow-keyboard-only` names it.

`upload` regenerates the VDFs and runs steamcmd `+run_app_build`. Credentials
are `STEAM_USERNAME`, `STEAM_PASSWORD`, and `STEAM_TOTP` in the environment.
They do not belong in `Regia.toml`, a VDF, a log, or a command you commit.
`--dry-run` prints the invocation and does not upload. `--branch default`
publishes to the branch players get and needs `--live` plus confirmation
(`--yes` in CI). There is no `--retries` field on this uploader.

Still open on Steam Input: action-set layers, more than one player on Steam
Input controllers, Steam's per-origin glyph images, and the Deck's floating
text input. Attaching user-generated content to a leaderboard entry on the
real client is not available yet (ADR-2340).

## Finished check

1. Step 2 prints `connected=true sealed=true` and exits 0, or step 3 shows the
   client in the server's player list and the link sealed (`net.crypto`).
2. A second client with a different `PROTOCOL_VERSION` is refused with
   `version_mismatch` or `protocol_mismatch`.
3. `regia package --server --dry-run` removes presentation assets and keeps
   levels, prefabs, and `net-manifest.ron`.
4. A `bad` conditioner run still reaches `in_game`, and a kick plus a ban
   removes the peer.
5. Voice, when you ship it: `voice_chat.test_source` produces a speaker in
   `voice_chat.status` with no microphone. A mic call without consent returns
   `consent_required`.
6. Steam, when you ship it: mock tests pass, and the owner sheet in spec 24
   § 4 is filled. An empty sheet means the real session is not done.

## Where the detail lives

| Topic | Page |
|---|---|
| Sockets, replication, prediction, relevance, QUIC budgets | [Networking](../../user-guide/networking.md) |
| Server package, console, lobbies, conditioner, PIE | [Dedicated servers](../../user-guide/dedicated-server.md) |
| Agent methods and error codes for the host | [Net dedicated server](../../agents/net-dedicated-server.md) |
| `session.host` / `session.connect` | [Net session](../../agents/net-session.md) |
| Voice methods | [Voice chat](../../agents/voice-chat.md) |
| Steam runtime and upload | [Steam](../../user-guide/steam.md) |
| Owner two-account procedure | [Steam real run](../../user-guide/steam-real-run.md) |
| Lockstep, rollback, desync | [Lockstep and rollback](lockstep-and-rollback.md) |
