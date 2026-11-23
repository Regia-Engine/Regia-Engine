# Net session (agents)

The session lifecycle of `regia_net` (spec 77, ADR-1663): every state a connection goes through from "connect" to
"disconnected because ...", the join handshake, level travel, reconnecting, bans, streams, the network conditioner
and simulated clients. Guide topic `net-session`. The dedicated-server host on top of it is
[net dedicated server](net-dedicated-server.md); the model and replication underneath are [net model](net-model.md)
and [net replication](net-replication.md).

A game drives one `NetSession` resource (`regia_net::session::NetSession`, added by `NetSessionPlugin`). It is idle,
a server (listen or dedicated) or a client. The session runs once per frame in `PreUpdate` after `NetSet::Receive`;
every transition happens there, so with the loopback hub's simulated clock a whole lifecycle replays identically.

## Client states

| State | Entered when | Does | Leaves when | Timeout (default) |
|---|---|---|---|---|
| `disconnected` | start, or after any failure | nothing; `reason` says why | `session.connect` | — |
| `connecting` | `session.connect` | transport connect | transport up → `handshaking` | 10 s → `timeout` |
| `handshaking` | transport up | sends `Hello`; waits for `Welcome` or a refusal | `Welcome` → `loading`; refusal → `disconnected(reason)` | 10 s + 10 s authenticator allowance (no startup grace before `Welcome`) → `timeout` |
| `loading` | `Welcome` or `TravelBegin` | loads the level; sends `LevelReady` | `LevelReady` sent → `joining` | 120 s → `level_load_failed` |
| `joining` | level loaded | receives the join snapshot | the `InGame` marker → `in_game` | 30 s → `timeout` |
| `in_game` | join complete | plays | travel → `loading`; leave → `disconnecting` | no authentic packet for 20 s → `timeout` |
| `disconnecting` | `session.disconnect` | three goodbyes (one per tick) | → `disconnected(reason)` | — |

## Server peer states

| State | Meaning |
|---|---|
| `handshaking` | Transport up, no valid `Hello` yet. At most `session.max_handshaking` (32) links; further transports are closed. |
| `loading` | `Welcome` sent; the player's `NetPlayer` entity exists; only keepalives are sent. |
| `joining` | `LevelReady` received; the join snapshot is on its way. |
| `in_game` | The client acknowledged the `InGame` marker. `PlayerJoined` fires. |
| `grace` | Dropped unexpectedly from `in_game` (`timeout`, `transport_error`, `suspended`); slot reserved, entity kept with `NetDropped`. |
| `gone` | Left, kicked, timed out or grace expired; `PlayerLeft { reason }` fires and the entity is despawned. |

Either side sends a keepalive after one second without sending anything.

## The handshake

A `Hello` carries the password, so it travels only on an encrypted link: see [Net transport
encryption](net-crypto.md).

The server checks a `Hello` in this order and refuses at the first failure (`Refused { reason, detail }`):

| # | Check | Refusal | What the player is told |
|---|---|---|---|
| 1 | The client's oldest protocol is one this server speaks (`MIN_NET_PROTOCOL_VERSION`..=`NET_PROTOCOL_VERSION`; each connection then falls back to what it speaks, see [net state channel](net-state-channel.md)) | `version_mismatch` | "Your game version does not match the server's. Update the game." (detail: both versions) |
| 2 | Protocol hash equal | `protocol_mismatch` | "Your game build is not compatible with this server." (detail: both hashes; in dev builds also `differing types: <type path> (missing on client \| missing on server \| different), …`) |
| 3 | Content manifest (gameplay content and mods) | `content_mismatch` | "Your game content differs from the server's:" then the detail lines, e.g. `pack:dlc_harbor: missing on client`, `mod:speedrun: different (client 1.1.0, server 1.0.0)` (at most 32, sorted) |
| 4 | Password, then the authenticator | `bad_password` / `auth_failed` | "Wrong password." / "The server could not verify your account." |
| 5 | Ban list | `banned` | "You are banned from this server: <reason> (until <time> / permanent)." |
| 6 | Reconnect token (valid: resume the peer's slot, from `grace` or by taking over its live `loading` / `joining` / `in_game` link, skip capacity; invalid: `TokenRefused { link }`, then a normal join) | — | — |
| 7 | Capacity (players, not peers; reserved slots) | `server_full` / `too_many_local_players` | "The server is full." / "Too many players on this device (at most 8)." |

The manifest check is bidirectional and includes mounted mods: a client with a mod mounted is
refused against a server without it (`mod:<id>: missing on server`) and vice versa
(`mod:<id>: missing on client`). This is deliberate — client and server simulate the same world,
so their mounted content sets must match exactly (spec 77 § 4.1). A mounted mod changes the
manifest's content hash but never the protocol hash, so it can never pass as "the same build".
In dev builds only, `dev_allow_content_mismatch: true` in `config/net.ron` downgrades the refusal
to a logged warning (never for a `manifest_version` difference); shipping builds have no override.

The first 16 bytes of every `Hello` (`RGNH`, version, hash) never change, so a client from another protocol version
whose body this server cannot decode still gets `version_mismatch`. Player names are trimmed; an empty name or one
longer than 32 characters is refused with `invalid_message`. Last, the game's `SessionRules::on_player_joining` may
refuse with a text, sent as `kicked`. While draining, every join is refused with `server_shutting_down`.

On Android a *listen* server (not dedicated) serves at most `replication.android_listen_max_clients`
players from `config/net.ron` (default 8, `0` reads as 8) even when `max_players` is higher: the extra
players are refused with `server_full` (spec 72 § 14). Other platforms and dedicated servers use
`max_players` alone.

## Joining a running game and travel

- Join snapshot, in this order: level entities removed since the level loaded, one record per relevant-everywhere
  (`sent_to_all` profile) entity and per entity the player's peer owns, then the `InGame` marker with the capture
  tick. The client despawns the removed level entities before its first `in_game` frame. The records name the
  entities (`NetId`); their state arrives in the replication join frame, where these entities go **outside** the
  spawn budget, while every other relevant entity streams in at `spawns_per_send` per send tick (ADR-7212). A join
  into a level with 300 entities: the player's character and the game state at once, the rest over about 13 send
  ticks. `session.state` → `join_snapshot` counts snapshots, removed entities (and how many the client found),
  records, and (server) capture passes over the replication state — one pass refreshes every loading peer whose
  inputs changed, so a quiet frame adds none.
- Travel (`session.travel`, console `level` / `next`, match end): the server picks a new `travel_id`, sends
  `TravelBegin` and moves every peer to `loading`. Each peer whose `LevelReady` names the current `travel_id` joins
  the new level on its own; slower peers never block faster ones. A stale `LevelReady` is ignored; a travel during
  loading restarts loading with the new level. Peer ids and player ids are kept.
- What a travel despawns: every replicated entity except those with `NetPersistAcrossTravel` (player entities and
  remote player controllers have it; add it to a team or score entity). Persistent entities keep their `NetId` on
  the server and on every client. `TravelBegin` names the persistent `NetId`s, so a client drops every other copy
  of the old level in the frame the travel arrives, not when the despawn records do (ADR-7259; a server with more
  than 8192 persistent entities sends no list and its clients wait for the records). Copies of entities the new
  level already spawned are kept. The drop also floors the old level's ids (ADR-7295): on the
  reliable tick stream, where `TravelBegin` overtakes the replication stream, a record of an id
  below the watermark still in flight is dropped and counted instead of showing the copy again
  for the ticks until the despawn records arrive; each despawn record clears its id, as on the
  state channel. `session.state` (server) → `travel { despawned, persistent, persistent_ids }`
  reports the last travel; on a client `travel { despawned, persistent, tombstoned }` is what the
  last `TravelBegin` dropped and kept, with `tombstoned` the cumulative count of records dropped
  by a tombstone (a despawn or out-of-scope notice, or the travel floor — not only old-level
  travel records).
  A pawn the rules spawned is not persistent unless the game marks it: re-spawn it in the new
  level.
- Mods in the manifest: each mounted mod presents its id, its version text and its content hash. The version is the
  `version` of the `luamod.ron` at the mod's root (empty when the mod has none; a mod manager can set one with
  `ModsVfs::set_mod_version`). `session.state` → `mods [{ id, version }]` lists them on every role. A builds-differ
  note: the session message layout is part of the protocol hash, so a build from before ADR-7259 and one after it
  refuse each other with `protocol_mismatch` (dev builds name `regia_net::session::wire`).

## Leaving, kicks, bans, reconnect

- Kicks remove one player (`peer.local`); the peer is disconnected with `kicked` when it was its last player.
- Bans (`bans.ron`, format: `docs/formats/bans-ron.md`): by platform id, by address or CIDR range, or by name
  (weakest). Expired entries never match.
- Reconnect grace (`session.reconnect_grace_s`, 60 s; 0 disables): an unexpected drop from `in_game` keeps the slot,
  the player entity (with `NetDropped`) and its `PeerId`. A `Hello` with the reconnect token from the last `Welcome`
  resumes it (`PlayerResumed`), even on a full server; a ban added meanwhile refuses the resume. After the grace a
  return is a new join with a new `PeerId`. A quit or a kick never enters the grace; a `suspended` client (Android)
  does. The app layer sends that goodbye as soon as the OS suspends it (frames stop before the next tick would, so the
  session cannot wait for one) and, when the process survives the pause, reconnects on resume with the last welcome's
  token, so the same `PeerId` comes back while the grace lasts (spec 77 § 16).
- Live-token takeover (ADR-8180): the token also works while the server still holds the old link as `loading`,
  `joining` or `in_game` (interrupted or not). A client that comes back before the server's own timeout (one-way loss,
  a game that reconnects at once, a restarted process on the same port) keeps its `PeerId` and its slot: no second
  peer, never `server_full` from its own old slot. The old link is dropped without a goodbye and its streams fail with
  `link_replaced`; `PlayerResumed` fires and the peer reloads the level as for a grace resume. A same-address rejoin of
  a quiet encrypted peer ends the old link as `timeout`, so an `in_game` player enters the grace.
- Resume transport: `NetSession::resume` (the Android/Deck `resumed` hook) reconnects over the transport the session
  connected with: lobby clients over the lobby's transport (Steam sockets), `connect` over UDP, `connect_via(opener,
  ..)` over whatever `opener` opens (QUIC, a custom socket). A `connect_on` without `set_socket_opener` resumes over UDP.
- Rebinds: when the transport moves an encrypted peer to a new address (NAT rebinding), `session.state` `addr`, the
  peer rows and `ban_player` follow it.

## Streams

`NetSession::send_stream(to, kind, bytes)` sends up to 64 MiB reliably in chunks, paced to at most
`session.stream_share` (25 %) of the connection's per-tick budget so gameplay keeps flowing; at most 4 in flight per
connection. The receiver gets progress, complete and failed events (`StreamEvent`); a drop fails every stream in
flight and nothing is resent after a reconnect. Larger streams fail with `stream_too_large`, a fifth with
`too_many_streams`. On the receiver a stream's `Failed` event says `stream_timeout` when the connection's incoming
streams moved less than 256 bytes per second over `session.stream_idle_timeout_s` (30), or `stream_buffer_full` when
the server's incoming streams passed `session.max_stream_buffer_bytes` (512 MiB).

Flow control (ADR-8180): a reliable window the transport refuses with backpressure (or its per-second reliable cap)
is not a drop. The session holds the messages and sends them on the next flushes; streams to that peer pause while
the window is half full or the peer is interrupted (both read `backpressured: true` in the peer's `session.state` row);
`SessionServer::try_send_game` hands the bytes back meanwhile so a game can hold optional traffic. Only a held backlog
past 8 MiB (`MAX_DEFERRED_RELIABLE_BYTES`) ends the link with `reliable_overflow`, and an overflow while the peer is
interrupted is the link's fault: the player enters the grace.

## Network conditioner

Dev builds. Per direction (`outbound` = this process sends, `inbound` = it receives), seeded so the same traffic
gives the same packet log. Effects in order: loss, delay plus uniform ± jitter, reorder (a chosen packet gets
`reorder_extra_ms`, default 20, more delay), duplication (the copy draws its own delay), then the bandwidth bucket.
It shapes loopback and simulated clients; UDP endpoints keep the transport conditioner of
[`net.conditioner_set`](net.md). Read it with `net.conditioner_get`, change it with `net.conditioner_apply` (spec 77
§ 14 uses these names since ADR-2413; protocol method names have one dot, ADR-1663 (19)).

| Preset | Latency | Jitter | Loss | Duplicate | Reorder |
|---|---|---|---|---|---|
| `off` | 0 | 0 | 0 | 0 | 0 |
| `lan` | 1 ms | 0 | 0 | 0 | 0 |
| `good` | 30 ms | 2 ms | 0.1 % | 0 | 0 |
| `average` | 60 ms | 8 ms | 1 % | 0 | 0 |
| `bad` | 120 ms | 20 ms | 3 % | 0 | 1 % |
| `terrible` | 250 ms | 50 ms | 8 % | 2 % | 3 % |

Fields of one direction: `latency_ms`, `jitter_ms`, `loss_pct`, `duplicate_pct`, `reorder_pct`, `reorder_extra_ms`,
`bandwidth_bytes_per_s` (0 = no cap).

Shipping builds (no debug assertions) compile the shaper out (spec 77 § 10.1): loopback and simulated clients pass
packets through unshaped, and `net.conditioner_get` reports `available: false`.

## Simulated clients

Dev builds, server side: `net.sim_clients` (or `NetSession::add_simulated_clients`) creates in-process clients that
join like real ones over the loopback link and render nothing. Inputs: `idle` (keepalives only), `random` (one
seeded 8-byte game message per tick), `recording` (frames from a `*.siminput.ron` file,
format: `docs/formats/siminput-ron.md`, one per tick, looping; `recording` is relative to the project, or absolute
under the project or the temp dir, else `path_outside_project`: [Request paths](safety.md#request-paths), ADR-7388). They connect in batches of 16 every 100 ms to
stay under `max_handshaking`; at most 256. `remove` takes the newest first; `clear` removes all.

Play-in-editor with several clients (spec 77 § 10.4) and the per-connection stats overlay (§ 10.2) are built: see [PIE with several clients and net stats](pie-net.md).

## Methods

| Method | Mode | Params | Result |
|---|---|---|---|
| `session.state` | read | — | client `{ role: "client", state, reason?, detail?, server, peer_id, level, travel_id, encryption, sealed, server_key, join_snapshot: { snapshots, removed, removed_applied, records }, travel: { despawned, persistent }, mods: [{ id, version }] }`; server `{ role: "server", session_id, level, travel_id, dedicated, peers: [{ peer, players, names, state, addr, interrupted, backpressured }], players, encryption, server_key, join_snapshot: { snapshots, removed, removed_applied, records }, travel: { despawned, persistent, persistent_ids }, mods: [{ id, version }] }`; idle `{ role: "idle", state: "disconnected", mods: [{ id, version }] }` |
| `session.host` | play | `{ mode: "listen"\|"dedicated", port? (default 7777, 0 = any free port), level?, max_players?, password?, encryption? }` | `{ session_id, port }` |
| `session.connect` | play | `{ address \| join_code, password?, players?, encryption?, server_key?, allow_unpinned? }` | `{ state }` |
| `session.disconnect` | play | — | `{ state }` (`draining` on a server) |
| `session.travel` | play (server) | `{ level }` | `{ travel_id }` |
| `session.manifest` | read | — | this process's content manifest (`format_version`, versions, hashes, `packs`, `mods`) |
| `net.conditioner_get` | read | — | `{ outbound, inbound, seed, presets, available }` (`available`: false in shipping builds) |
| `net.conditioner_apply` | play (dev builds) | `{ preset?, outbound?, inbound?, seed? }` | the active settings |
| `net.sim_clients` | play (server, dev builds) | `{ action: "add"\|"remove"\|"clear", count?, input?: "idle"\|"random"\|"recording", seed?, recording? }` | `{ simulated }` |

```json
{ "method": "session.host", "params": { "mode": "listen", "port": 0, "level": "levels/arena.level.ron" } }
→ { "session_id": 1234567890, "port": 40211 }

{ "method": "net.sim_clients", "params": { "action": "add", "count": 4, "input": "random", "seed": 7 } }
→ { "simulated": 4 }

{ "method": "session.state" }
→ { "role": "server", "session_id": 1234567890, "level": "levels/arena.level.ron", "travel_id": 1,
    "dedicated": false, "peers": [{ "peer": 1, "players": ["1.0"], "names": ["sim-1"], "state": "in_game",
    "addr": "127.0.0.1:20000" }] }
```

Verify with `play.step` (or frame steps) and read `session.state` again: joins take a few ticks.

## Errors

The errors in this table are `F0002` with `data.reason`. A bad parameter (for example `session.host`
`mode` other than `listen` or `dedicated`) is `F0002` with `data.hint` and no `data.reason`:

| Reason | When |
|---|---|
| `net_not_running` | The app has no `NetSession` (`NetSessionPlugin` not added). |
| `already_in_session` | `session.host` / `session.connect` while a session runs. |
| `net_not_server` | A server-only method (`session.travel`, `net.sim_clients`) on a client or idle session. |
| `level_not_found` | `session.travel` with an empty or unknown level. |
| `conditioner_unavailable` | `net.conditioner_apply` in a shipping build. |
| `sim_clients_unavailable` | `net.sim_clients` in a shipping build. |
| `recording_invalid` | `net.sim_clients` `input: "recording"` with a missing file or a `*.siminput.ron` that is not version 1. |
| `preset_unknown` | `net.conditioner_apply` names a preset that does not exist. |
| `sim_clients_limit` | More than 256 simulated clients. |
| `join_code_unsupported` | `session.connect` with a join code: resolve it through the relay and connect by address. |
| `transport_error` | Binding or opening the transport failed (detail: the OS error). |
| `password_needs_encryption` | `session.host` with a password on a non-loopback bind, or `session.connect` with a password to a non-loopback server, while `encryption` is `Off` (spec 77 § 5, ADR-1901). |
| `password_needs_encryption` | `session.host` / `session.connect` with a password and `encryption: off` on a non-loopback address ([net-crypto](net-crypto.md)). |
| `net_known_hosts_unreadable` | `session.connect`: the known-hosts store is damaged; it is left as it is (pass `allow_unpinned` to connect anyway). |
| `net_known_hosts_newer` | `session.connect`: the store's `format_version` is newer than this build reads. |
| `net_bad_host_id` | `session.connect`: the address does not make a host id (never for an `ip:port`). |

When a connect attempt ends, `session.state` reports the reason code in `reason` (and `detail`).

### Server key pinning (ADR-3283)

`session.connect` trusts the server's key on first use, the way [net-trust](net-trust.md) describes for `NetLink`: the store is
`user://known_hosts.ron` (`NetSessionPlugin` sets it; a game may call `NetSession::set_known_hosts`), and the host id is the
address you pass.

- First connection to an address: the client records the key the server proved, then sends `Hello`.
- Later connections must show the same key. Another key ends the attempt with `reason: "transport_error"`,
  `detail: "net_host_key_changed"` **before `Hello` (and its password) leaves the client**; the store keeps the old key.
  Check the key with the server's operator, then `net.trust` the new key or `net.forget` the host.
- `server_key` wins over the store, which is then neither read nor written.
- `allow_unpinned: true` skips the store for that call (LAN development against a listen server that makes a new key every
  run). Without a `server_key`, an attacker in the middle can then read the password. A listen host normally does not make a new
  key every run: it keeps one in `user://listen-server.key` (ADR-4936), so this is the escape hatch for a game that manages its
  own identity or has no `user://` mount.
- A plain link (loopback with `encryption: auto`, or `encryption: off`) has no key: nothing is recorded.

```json
{ "method": "session.connect", "params": { "address": "203.0.113.7:7777", "password": "hunter2" } }
→ { "state": "connecting" }
{ "method": "session.state" }
→ { "role": "client", "state": "disconnected", "reason": "transport_error", "detail": "net_host_key_changed", ... }
```

## Disconnect reasons

Codes are stable (agent error codes and analytics keys). `regia_net::session::DisconnectReason` has one variant per
code; variants with a detail carry it as a `String`.

| Code | Decided by | Meaning |
|---|---|---|
| `client_quit` | client | The player left (`session.disconnect`). |
| `server_shutting_down` | server | The server is draining; new joins are refused too. |
| `timeout` | either | No packets within the current state's timeout (or the grace expired). |
| `kicked` | server | An admin or a game rule kicked the player (detail: the reason). |
| `banned` | server | A ban entry matched (detail: reason and expiry). |
| `server_full` | server | No free player slots. |
| `too_many_local_players` | server | More than 8 local players declared. |
| `version_mismatch` | server | The client's oldest protocol is one this server does not speak (detail: both). |
| `protocol_mismatch` | server | Protocol hashes differ (detail: both). |
| `content_mismatch` | server | Content manifests differ (detail: up to 32 items, or `manifest_version`). |
| `bad_password` | server | Wrong password, or a try inside this address's backoff wait: from the tenth wrong password or admin token of one address (one IPv4 address or IPv6 /64, `MAX_AUTH_STRIKES`), each wrong try makes the address wait 4 s, doubling per further wrong try up to 30 s (`auth_backoff_ms`), before its next try is checked; tries inside the wait are refused unchecked and not counted. An address is slowed, never locked out (a household behind one NAT address keeps getting in), and its strikes are forgotten a minute after its last wrong try (ADR-5993). |
| `auth_failed` | server | The authenticator refused or timed out, three wrong remote-admin tokens on one connection, or an admin login from an address inside its backoff wait (see `bad_password`). |
| `invalid_message` | server | A to-server message failed validation with `Kick` (spec 70), or invalid player names. |
| `malformed_packet` | either | Bytes that did not decode, or a broken stream. |
| `flood` | server | The peer sent faster than its flood limits (spec 70). |
| `reliable_overflow` | either | The reliable queue bound was hit (spec 70). |
| `level_load_failed` | either | The level could not be loaded, or loading took longer than the timeout. |
| `transport_error` | either | The transport reported a failure (detail: its text). |
| `suspended` | client | The app was suspended by the OS (Android); enters the reconnect grace. |

## Settings

`config/net.ron` section `session` (format: `docs/formats/net-ron.md`), applied by `NetSessionPlugin` when a `NetSettings` resource is inserted before it (the dedicated host's `server.toml` encryption wins): `timeout_ms`, `interrupt_ms`, `start_grace_ms`, legacy `timeout_s`, `connect_timeout_s`,
`handshake_timeout_s`, `loading_timeout_s`, `joining_timeout_s`, `auth_timeout_s`, `max_handshaking`,
`reserved_slots`, `reconnect_grace_s`, `stream_share`, `stream_idle_timeout_s`, `max_stream_buffer_bytes`, `dev_allow_content_mismatch`, `encryption` ([net-crypto](net-crypto.md)). The content manifest is
`content/net-manifest.ron` in packaged builds (format: `docs/formats/net-manifest-ron.md`).

## Players, hand-off and the stats overlay

Remote players' join and leave reach `GameRules`, a leaving peer's entities are handed on, and ping and loss per
player are a resource and an in-game overlay: see [Net session players](net-session-players.md)
(`net.player_stats`, `net.stats_overlay`, `net.handoffs`).

### Interrupted sessions (ADR-7950)

`session.state` retains the lifecycle state (`in_game`, `loading`, etc.) and adds
`interrupted` on a client and in each server peer row. The flag is true after 1500 ms of
validated-traffic silence by default; restoration clears it without a reconnect or new id.
`SessionSettings` has `interrupt_ms`, `timeout_ms` (20000) and `start_grace_ms` (30000).
`timeout_s` is a legacy override only when `timeout_ms` remains at its default; a
`config/net.ron` that names `timeout_ms` wins over its `timeout_s` at any value. NetSession
passes the resolved policy to both underlying transport configs; application-only heartbeat
silence cannot expire a session that continues receiving authentic transport packets.
No new method or error code is introduced. `net.status`/`net.peers` show the equivalent datagram
observations; keep the existing distinction between session peer ids and transport link ids.

## Automatic reconnect (ADR-8352)

A session client whose server went silent past the timeout reconnects by itself with its reconnect token (spec 120 § 10.3):
attempts 1 s, 2 s, 4 s and then every 8 s apart with jitter for `reconnect_grace_s`, over the session's `SocketOpener` (a
lobby's Steam sockets), UDP, or the caller's own socket after `connect_on`. Events: `ClientSessionEvent::PeerInterrupted`
once, `Reconnecting { attempt }` per attempt, then `Welcomed` and one `PeerRestored`, or `ReconnectFailed { attempts,
reason }` and then `Disconnected { reason }`. `NetSession::is_reconnecting()` / `reconnect_status()` show progress;
`set_reconnect_policy(ReconnectPolicy::off())` restores the end at the timeout. The server side is the existing grace slot
(`PlayerResumed`). No new method or error code.

## One-round-trip transport resume (wire 6, ADR-10088)

Every `resume: true` connect of a session client (the automatic reconnect, `resume`, `resume_on`, the game's own
`connect`, direct or by join code) presents the transport's resume ticket and resumption credential of its last
accept, beside the session's reconnect token ([net-link](net-link.md), "One-round-trip resume"). A session host's
transport keeps a timed-out player's ticket for `reconnect_grace_s` (0 keeps none), so on a sealed link the transport
connect is one round trip and comes back under the same `LinkId`; the `Hello` with the reconnect token then resumes
the player as before. Events and error codes are unchanged; no new agent method.

- A player in grace: the link comes up again, the token resumes the slot (`PlayerResumed`).
- An early return (the client reconnects before the host's timeout): the transport takes the live connection over in
  place. The session core detaches the player from the link and keeps its state while the link waits for its `Hello`;
  the token resumes it with no `PlayerDropped`, as a takeover from a new link does, and the link is not closed. Any
  other outcome of that link (another player's `Hello`, a refusal, `handshake_timeout_s`, the link going down) is the
  loss of the old link: grace from `in_game`, a leave otherwise.
- A link the session closes itself while it keeps the player in grace (a suspend's goodbye, its own silence timeout, a
  reliable overflow on a silent link): the host closes it with `NetServer::kick_held`, which tells the far end as a kick
  does and holds the peer dormant as a timeout does, so `resume` / `resume_on` and the automatic reconnect are
  one-round-trip resumes too. `SessionServer::grace_holds_link(link)` is true for such a link.
- The transport's hold never outlives the session's: for a timed-out player the session does not keep in grace (one
  still loading its first join) the host ends the hold at once (`NetServer::forget_dormant`).
- The full handshake (three round trips, with the 16-byte ticket) remains for a plain link, for `reconnect_grace_s` 0,
  and when the host refuses the resume (`resume_refusals` in the transport's totals).
- With a grace above 0 the transport's per-address holds apply to session hosts too: a player that leaves keeps its
  slot of `max_peers_per_ip` for the grace ([net-trust](net-trust.md)).

To check it: `NetSession::transport_totals()` on the host shows `dormant` and `resumed_one_rtt` rise by one per outage and
`resume_fallbacks` stay 0; on the client `resume_inits` and `resumed_one_rtt` rise together.
`cargo test -p regia_net --test session_resume_one_rtt` runs the outage, the early return, the suspend, the plain-link
fallback, the session core's link cases and the two transport calls in virtual time.
