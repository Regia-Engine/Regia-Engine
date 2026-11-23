# 77 — Session lifecycle

Status: proposed (ADR-1599). Plan items D1 (sessions, peer health and bans from the `ir-net` extraction), D7 (content handshake), and the
"network conditioner, PIE with N clients" tools of `22-tier2.md` § T2.3; rung v0.3.x (b) start, v0.5.0 complete. Crate:
`regia_net::session`. Every other net spec depends on the states defined here (`InGame`, travel, the join snapshot).

Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_net` does not exist in the tree; the session layer is new. Built and used: `regia_framework::GameRules`
(`on_player_joined`, `on_player_left`, `rules.rs`), `regia_app` fixed time, the mod manager, `regia_steam` (join requests), the editor's
play-in-editor (`regia_editor`), `regia_core::id::Name`. The `ir-net` extraction (transport, tokens, encryption, health) is a separate
lane's input; this spec is the behavior contract above it.

Builders read this file, specs 70-76 (netcode batch, ADR-1599), plus `14-scene-and-framework.md` (levels, `GameRules`) and `12-assets.md` § 5 (paks, mounts).

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-networking/runtime-networking-transport-debugging` (handshake
behavior, version fallback, connection close results, packet simulation, simulated connections, flood tracking),
`runtime-networking/runtime-networking-overview`. Re-derived in Regia terms (§ 18); no wording, identifiers, settings or command names
carried over.

Relation to the `ir-net` extraction (`master-plan.md` § 3.5b): the extraction supplies the transport, connect tokens, encryption, peer health,
the conditioner and join codes. This spec is the behavior contract above them. Where the extraction's behavior differs, the builder writes
an ADR before choosing.

## 1. Purpose and scope

A session is one server's game with its connected clients. This spec defines every state a connection goes through, from "connect" to
"disconnected because ...", and what each side does in each state:

- the client and server state machines and their timeouts;
- the handshake: protocol version, protocol hash, content manifest (and how a mismatch is explained to the player);
- authentication, passwords, capacity;
- joining a running game (the join snapshot), level travel, late join;
- leaving: disconnect reasons, kicks, bans, timeouts, reconnecting within a grace period;
- large reliable transfers (streams);
- hooks for game rules;
- the developer tools: network conditioner, connection stats, simulated clients, and play-in-editor with several clients.

Not in scope: lobbies and matchmaking (D6), voice chat, NAT traversal and relays (the extraction), store authentication backends (a
pluggable trait only), host migration.

## 2. Terms

| Term | Meaning |
|---|---|
| Session | One server process's game and its connections, from `host` to shutdown. Has a random 64-bit session id. |
| Connection state | Where one client is in the lifecycle (§ 3). The server keeps one per peer; the client keeps its own. |
| Protocol hash | 64-bit hash over every code-registered replicated component, message and input type, with their codecs (specs 70-73). Content (including `.netprofile.ron` quantization) is covered by the content hash instead. |
| Content manifest | `net-manifest.ron`: engine and game versions, protocol version and hash, and the gameplay content hash with per-pak and per-mod hashes. |
| Join snapshot | What a client receives when it enters a level: its welcome data, removed level entities, and the first relevant spawns. |
| Travel | The server moving the whole session to another level. |
| Reconnect grace | A period after an unexpected drop during which the same player may rejoin into the same slot. |

## 3. State machines

### 3.1 Client

| State | Entered when | Does | Leaves when | Timeout (default) |
|---|---|---|---|---|
| `Disconnected(reason?)` | start, or after any failure | nothing | `connect` called | — |
| `Connecting` | `connect(target)` | transport connect (token, encryption) | transport established → `Handshaking` | 10 s → `Disconnected(timeout)` |
| `Handshaking` | transport up | sends `Hello` (§ 4); waits for `Welcome` or a refusal | `Welcome` → `Loading`; refusal → `Disconnected(reason)` | 10 s plus `auth_timeout_s` (no startup grace before admission, ADR-7950) |
| `Loading` | `Welcome`, or `TravelBegin` | loads the named level locally; sends `LevelReady` when done | `LevelReady` sent → `Joining` | 120 s (`level_load_failed`) |
| `Joining` | level loaded | receives the join snapshot (§ 6.1) | the first `InGame` marker arrives → `InGame` | 30 s |
| `InGame` | join complete | plays: replication, input, messages | `TravelBegin` → `Loading`; leave → `Disconnecting` | no authentic packet for 20 s → `Disconnected(timeout)` |
| `Disconnecting` | `disconnect()` or a server refusal mid-game | sends a goodbye with the reason (3 copies, unreliable); stops sending input | after 3 ticks → `Disconnected(reason)` | — |

### 3.2 Server (per peer)

| State | Meaning | Server behavior |
|---|---|---|
| `Handshaking` | transport up, no valid `Hello` yet | validates `Hello` (§ 4-§ 5); at most `session.max_handshaking` (default 32) peers in this state, further transports are closed |
| `Loading` | `Welcome` sent; client loading | sends nothing but keepalives; its player controller exists (spawned at `Welcome`) but is not yet relevant to others unless the game makes it so |
| `Joining` | `LevelReady` received | sends the join snapshot; the player's entities become relevant to others |
| `InGame` | join snapshot acknowledged up to its `InGame` marker | normal replication, input consumption, message delivery |
| `Grace` | dropped unexpectedly from `InGame` | player controller kept, possessed entity kept or frozen per `SessionRules` (§ 9); slot reserved; § 7.4 |
| `Gone` | left, kicked, timed out, or grace expired | entities owned by the peer are despawned or handed to the server per `SessionRules` (§ 9); the peer id is retired |

- Keepalives: either side sends a keepalive when it has sent nothing for 1 s. A connection in `InGame` with nothing received for
  `session.timeout_ms` (default 20,000 ms) is dropped with `timeout`.
- The server never blocks on a peer: a peer in `Loading` for longer than the loading timeout is disconnected with `level_load_failed`.

### 3.3 Shared silence policy and interruption (built, ADR-7950)

`TimeoutPolicy { interrupt_ms, timeout_ms, start_grace_ms }` resolves once at the owning
layer. `NetSession` passes `SessionSettings::timeout_policy()` to its transport server's
`HealthPolicy` and transport client's `ClientConfig`. `NetLink` passes `LinkOptions::timeout`
to both endpoints. A dedicated host applies `[net]` durations from `server.toml` to both its
session and replication game port. Configure both processes; this policy is not negotiated on
the wire. `connect_timeout_s`, authenticator, level-loading and join deadlines remain separate.

Defaults are 1,500 / 20,000 / 30,000 ms. Old RON seconds are read only from a file without
`timeout_ms`; a `timeout_ms` the file names wins at any value. Legacy Rust `session.timeout_s`
still works while `timeout_ms` keeps its default. Every layer clamps the policy into range
(`TimeoutPolicy::sanitized`); `server.toml` refuses out-of-range values. New files write
milliseconds only. `regia upgrade-project --write` migrates `config/net.ron` v1/v2 to v3 and
`server.toml` v1/v2/v3 to v4. This does not change wire framing.

Startup grace is a floor from admission, not another interval added to every outage. Before
game data, removal requires both the silence deadline and the startup floor to have elapsed.
Empty keepalives and tick synchronization do not end startup grace. The session adapter treats
its Hello/loading envelopes as startup until `InGame`. The floor starts at admission: a link that
never sends `Hello` is refused at `handshake_timeout_s`, so half-open links cannot hold the
`max_handshaking` slots for the grace. In-game silence removes at `timeout_ms` (inclusive), never because RTT or loss
exceeds a diagnostics threshold. The 12 s stall acceptance uses an explicit 10 s policy.

Interruption is orthogonal to lifecycle phase: it never changes identity, occupancy, loading,
join or reconnect deadlines. Both datagram ends emit `PeerInterrupted` and `PeerRestored` once
per outage; `LinkEvent` and session events carry them to games. Only validated connection
traffic restores the link; transport activity also refreshes session liveness when it carries
no session message. `net.status`, `net.peers`, `session.state` and the diagnostics overlay expose
`interrupted`. The bridge/session adapter applies the `NetInterrupted` marker to the silent peer's
owned entities and controllers on the server, and removes it on restore, leave or owner change.
Replicating it is opt-in (`NetPlugin::replicate_interrupted_marker`, every process) because it
changes the protocol hash. Proxy interpolation holds marked entities; authoritative game movement
should exclude the marker until restored.
Idle interruption probes back off from 250 ms to 4 s with deterministic bounded jitter.

## 4. Handshake

`Hello` (client → server, reliable, the first message after transport setup). Its first 16 bytes are a fixed prefix that every protocol
version keeps: magic `b"RGNH"`, `protocol_version: u32` (little endian), `protocol_hash: u64` (little endian). The server decodes the
prefix first, so it can answer `version_mismatch` or `protocol_mismatch` to a client whose later fields it cannot decode; the rest is
decoded only when both match.

```rust
pub struct Hello {
    pub protocol_version: u32,        // the oldest protocol the client speaks: MIN_NET_PROTOCOL_VERSION (ADR-1950)
    pub protocol_hash: u64,
    pub manifest: ContentManifest,
    pub player_names: Vec<String>,    // one per local player, 1..=8, each 1..=32 chars after trimming
    pub password: Option<String>,
    pub auth: Option<Vec<u8>>,        // opaque, for a `SessionAuthenticator` (§ 5)
    pub client_rate_bytes: u32,       // the client's own downstream limit; the server uses the lower of this and its setting
    pub reconnect_token: Option<[u8; 16]>,
}
```

The server checks, in this order, and refuses at the first failure with a `Refused { reason, detail }` message:

1. Protocol version between `MIN_NET_PROTOCOL_VERSION` and the server's `NET_PROTOCOL_VERSION` → else `version_mismatch` (detail: both
   versions). A connection then falls back to what the older side speaks through the `ACK` frame (spec 72 § 4, ADR-1950).
2. Protocol hash equal → else `protocol_mismatch` (detail: both hashes; dev builds add the list of differing registered type names).
   As built (ADR-7212): the dev server appends its type list (`\ntypes: path=fp32;…`, cut to the detail limit) and the dev
   client rewrites the detail to `differing types: <path> (missing on client | missing on server | different)`; no `Hello`
   change, so the wire and the protocol hash are unchanged.
3. Content manifest (§ 4.1) → else `content_mismatch` (detail: up to 32 differing items).
4. Password and authenticator (§ 5) → `bad_password` / `auth_failed`. (The authenticator supplies `platform_id`, which the ban check
   needs, so it runs first.)
5. Ban list (§ 7.3) → `banned` (detail: reason and expiry, if the entry has them).
6. Reconnect token, if present and valid → resume the peer's slot (§ 7.4), skipping the capacity check (the slot is still reserved): a
   `Grace` slot, or a slot whose old link the server still holds as `Loading`, `Joining` or `InGame` (interrupted or not; the live-token
   takeover of § 7.4, ADR-8180). Invalid or expired tokens raise `TokenRefused { link }` and the join goes on as a normal join.
7. Capacity → `server_full` (players, not peers) or `too_many_local_players`.

On success the server answers `Welcome { peer_id, session_id, server_tick, tick_hz, send_every_ticks, level, travel_id, motd, server_name,
reconnect_token, players: Vec<NetPlayerId> }` and moves the peer to `Loading`.

### 4.1 Content manifest

```ron
NetManifest(
    format_version: 1,
    engine_version: "0.5.0",
    game_version: "1.2.0",
    protocol_version: 1,
    protocol_hash: 0x8f3a51c2d9e04b17,
    content_hash: 0x0123456789abcdef,
    packs: [("base", 0x1111111111111111), ("dlc_harbor", 0x2222222222222222)],
)
```

At session start each process adds the mods it has mounted (`mods: [(id, version, hash)]`, from the mod manager) to its in-memory
manifest; the file on disk holds the packaged packs only.

- **Hashed kinds** (a fixed list, a subset of the server content of spec 76 § 3): levels, prefabs, `.netprofile.ron` profiles, data
  tables and other gameplay data assets, `config/*.ron`, collision meshes and heightfields, and navmesh data. Animation clips, skeletons,
  textures, audio, materials and every other presentation asset are not hashed, so a client with a different texture quality pack still
  joins, and the server pak (which has only some clips) hashes the same as a client pak.
- **Packs** are logical: the project's content packs (base, each DLC chunk; the chunk names of the content-chunks spec), not physical pak
  files. A pack's hash covers `(asset id, cooked content hash)` of its hashed-kind assets, sorted by asset id; `content_hash` covers all
  packs and mods in name order. The same asset in a different physical pak hashes the same.
- Mismatch detail: each differing pack or mod by name with "missing on client", "missing on server" or "different", sorted by name, at most
  32 items. The client UI shows them; the guide page has a ready-made text.
- Asset hash as built (ADR-1886 (6)): a cooked asset hashes by its source file plus `.meta`, not its cooked bytes (which
  differ per platform), and its id is the asset path; cooked assets with no source of their own (tiled terrain) fall back
  to their cooked bytes. `protocol_hash` is 0 in the file; each process stamps its own at session start.
- Packaged builds read the manifest from `content/net-manifest.ron` (written by `regia package`, spec 76 § 3). Dev builds compute it at
  session start from the dev cache (logged with its time); `session.dev_allow_content_mismatch: true` (dev only) downgrades a mismatch to a
  warning.
- As built (ADR-7212): mounted mods come from `ModsVfs` (id, empty version text, `ContentSetHash::short()`), read at plugin
  `finish` and whenever the mount revision changes; `NetSession::session_manifest` is what a host or client presents. The
  dev build's manifest is computed by the launcher: `regia run` (after its cook) and `regia server` write it from the dev
  cache with the same function, log the time, and pass `REGIA_NET_MANIFEST`; the runtime logs the file it read (hashing in
  the runtime would need the cook closure, which lives in desktop-only crates).
- As built (ADR-7259): a mod's version text is the `version` of the `luamod.ron` at its root (spec 84 § 3; folder, pak or
  in-memory mod), read once at mount into `ModInfo::version` (trimmed, at most 64 bytes, the manifest's wire limit); a mod
  without one presents empty text and still joins a server whose mod declares none; `ModsVfs::set_mod_version` lets a mod
  manager set it from elsewhere (a Workshop item). A mod whose version differs is listed as
  `mod:<id>: different (client <v>, server <v>)` (`no version` for empty text). `session.state` lists `mods [{ id, version }]`.
  Test: `tests/session_post05w3.rs` (acceptance 3 end to end, two apps).
- Migration note: `net-manifest.ron` is a new generated Regia format, `format_version: 1`. It is never edited by hand; a layout change bumps
  the version and both sides refuse other versions with `content_mismatch` (detail `manifest_version`).

## 5. Authentication, passwords and capacity

- Passwords: compared in constant time against `server.password` (spec 76 § 4). An empty server password accepts any client password.
  Passwords travel only over the encrypted transport (ADR-1901): with `encryption: Off`, a client refuses to send a password
  to a non-loopback server and a host refuses to take one on a non-loopback bind (`password_needs_encryption`). On a plain
  loopback link the check still runs.
- `SessionAuthenticator` (trait, optional, one per session): `fn check(&mut self, id: AuthRequestId, hello: &Hello, peer_addr: &PeerAddr)
  -> AuthResult` with `Accept { platform_id: Option<String>, reserved: bool }`, `Reject(&'static str)` or `Pending`. For `Pending`, the
  session calls `fn poll(&mut self, id: AuthRequestId) -> Option<AuthResult>` once per tick until it returns `Accept` or `Reject`, for up to
  `session.auth_timeout_s` (default 10 s; then `auth_failed`); the peer stays `Handshaking` meanwhile. Store-platform authenticators plug in here (D5/D6).
- `platform_id` (when an authenticator provides it) is what bans by player use (§ 7.3).
- Capacity counts players: a client with two split-screen players needs two free slots. Reserved slots (`session.reserved_slots`, default 0)
  are only usable by clients whose authenticator result marks them `reserved`.

## 6. Joining and travel

### 6.1 Join snapshot

When a peer becomes `Joining`, the server sends, reliably and in this order:

1. `level_removed`: `EntityGuid`s of level-placed replicated entities the server has despawned since the level loaded (spec 71 § 4.5).
2. Spawn records for relevant-everywhere entities (game state, spec 72 § 5.1), the peer's own controller and owned entities, regardless of the spawn budget.
3. An `InGame` marker carrying the server tick at which items 1 and 2 were captured.

The client applies 1 before its first `InGame` frame (removed level entities never flash into view), and enters `InGame` at the marker.
Other relevant entities then stream in through the normal spawn budget (spec 72 § 7), so a join into a busy level spreads its cost over a
few send ticks.

As built (ADR-7212): `NetSession` feeds the core's join source from replication each step for the peers in `Loading`
(`session::travel::join_snapshots_for`). Item 2 carries one 5-byte `JoinRecord` (own / game state, `NetId`) per join-critical
entity; their state goes in the replication stream's join frame, where the peer's own and `sent_to_all` spawns rank first and
pass the spawn budget while the connection's join window is open (one copy table per connection, so no second spawn path).
The client despawns local entities with the item-1 `EntityGuid`s before `InGame`. Counts: `session.state` `join_snapshot`,
`ReplicationCounters::join_critical_spawns`. Test: `tests/session_post05.rs`
(`join_frame_carries_own_and_game_state_entities_outside_the_spawn_budget`, acceptance 5's 300-entity part).

### 6.2 Level travel

- Only the server starts travel: `NetSession::travel(level)` (or the console's `level`/`next`, spec 76 § 6, or the host at match end, spec 76 § 5).
- Sequence:
  1. The server picks a new `travel_id`, broadcasts `TravelBegin { level, travel_id }` reliably, stops replication and input
     consumption, and moves every peer to `Loading`.
  2. The server despawns every replicated entity except those marked `NetPersistAcrossTravel` (player controllers have it by default;
     a game may add it to a team or score entity), unloads the old level, loads the new one.
  3. Each client despawns its replicated copies (except persistent ones, which keep their `NetId`), loads the level, sends
     `LevelReady { travel_id }`.
  4. Each peer whose `LevelReady` matches the current `travel_id` gets its join snapshot (§ 6.1) and continues to `InGame`, independently
     of slower peers. The server's simulation of the new level starts when the level is loaded on the server, not when clients are ready.
- A `LevelReady` with an old `travel_id` is ignored. A client still loading when another travel begins restarts with the new level.
- Clients that join during a travel receive the new level in `Welcome`.
- As built (ADR-7212): `NetPersistAcrossTravel` is the marker; the session's `NetPlayer` entities and the framework's remote
  player controllers carry it. The travel reopens every connection's join window and forgets the old
  level's removed entities. `session.state` reports the last travel's counts and persistent `NetId`s. Tests:
  `tests/session_post05_travel.rs` (acceptance 6's `NetId` part).
- As built (ADR-7259), step 3: `TravelBegin` carries `keep: Option<TravelKeep { below, ids }>`: the persistent `NetId`s
  and the first `NetId` the server had not assigned, read from the world in the step that sends the message and despawns
  the old level. `NetId`s are never reused, so the client drops at once every copy whose id is below `below` and not in
  `ids` (the despawn-record path, without the interpolation wait); copies of new-level entities and the persistent copies
  stay under their `NetId`s, and the server's despawn records arrive later to nothing. A server with more than 8192
  persistent entities, or a session core driven without a world, sends no list and clients wait for the despawn records.
  The session message layout is in the protocol hash (`SESSION_WIRE_REVISION` 2), so builds from before this refuse with
  `protocol_mismatch`. `session.state` (client) reports `travel { despawned, persistent }`. Test:
  `tests/session_post05w3.rs::the_client_holds_only_persistent_copies_at_travel_begin` (two apps, despawn records on a
  5-step delay line).
- As built (ADR-7295): the drop also installs a floor from the `TravelKeep` watermark: on the
  reliable tick stream (protocol 1), where `TravelBegin` overtakes the replication stream, spawn
  and update records of an id below the watermark (ids are never reused) still in flight are
  dropped and counted (`ClientReplication.state.counters.tombstoned`) instead of showing the copy
  until the despawn records arrive — including ids the client had not spawned when the travel
  landed; the persistent ids spawn, and each despawn record clears its id from the floor, as the
  notice replaces the point tombstone on the state channel. `session.state` (client) reports
  `travel { despawned, persistent, tombstoned }` (`tombstoned` is the connection-cumulative counter
  of every record dropped by a tombstone, despawn and out-of-scope notices included, not only the
  travel's old-level records). Tests: `tests/session_post05w4.rs` (protocol 1
  in memory, delay line; fails on the base) and `tests/session_post05w4_udp.rs` (two processes
  over real loopback UDP).

## 7. Leaving

### 7.1 Disconnect reasons

`DisconnectReason` is sent to the other side when possible and given to game code in `PlayerLeft` (server) and in
`Disconnected(reason)` (client). In Rust it is `enum DisconnectReason` with one variant per code (`ClientQuit`, `ServerShuttingDown`,
..., `TransportError(String)`); variants with a detail carry it as a `String`. Codes are stable (they are agent error codes and analytics
keys):

| Code | Who decides | Meaning |
|---|---|---|
| `client_quit` | client | The player left. |
| `server_shutting_down` | server | Draining (spec 76 § 5). |
| `timeout` | either | No packets within the timeout of the current state. |
| `kicked` | server | An admin or game rule kicked the player (detail: reason text). |
| `banned` | server | A ban entry matched (detail: reason, expiry). |
| `server_full` | server | No free player slots. |
| `too_many_local_players` | server | More than 8 local players declared. |
| `version_mismatch` / `protocol_mismatch` / `content_mismatch` | server | § 4. |
| `bad_password` / `auth_failed` | server | § 5. |
| `invalid_message` | server | A to-server message failed validation with `Kick` (spec 70 § 6.4; detail: the reason). |
| `malformed_packet` | either | Bytes that did not decode. |
| `flood` | server | Spec 70 § 6.6. |
| `reliable_overflow` | either | Spec 70 § 6.5 queue bound. |
| `level_load_failed` | either | The level could not be loaded, or loading took longer than the timeout. |
| `transport_error` | either | The transport reported a failure (detail: the transport's text). |
| `suspended` | client | The app was suspended by the OS (Android); unlike `client_quit` it enters the reconnect grace (§ 7.4). |

### 7.2 Kicks

`NetSession::kick(player, reason)` removes that one player and tells its peer (`kicked` with the reason). If it was the peer's last player,
the peer is disconnected with `kicked`. `NetSession::kick_peer(peer, reason)` removes all of a peer's players and disconnects it.

### 7.3 Bans

`bans.ron` on the server:

```ron
BanList(
    format_version: 2,
    entries: [
        (id: 1, kind: PlatformId, value: "steam:76561190000000000", until: None, reason: "cheating", added: 1793000000),
        (id: 2, kind: Address, value: "203.0.113.7", until: Some(1793600000), reason: "spam", added: 1793000000),
    ],
)
```

- `kind`: `PlatformId` (from the authenticator), `Address` (an IPv4 address, an IPv6 address or a `/prefix` range), or
  `Name` (exact player name, weakest, logged as such). `until` is a Unix time in seconds or `None` for permanent. Expired entries are
  ignored and removed on the next save. A running server stamps and judges `until` and `added` on its session clock
  (`NetSession::now_unix_s`: the wall second at session start plus the monotonic session clock), the clock its join check reads,
  on every path (console, agent, game port), never the system clock mid-run (ADR-5973).
- `Address` reach, `format_version: 2` (ADR-3283, ADR-4936): a bare IPv4 address bans that address. A bare IPv6 address bans its whole
  /64 — one subscriber rotates addresses inside it for free, so banning one address alone is no ban; write `addr/128` to ban exactly one
  IPv6 address. Any other prefix bans the range it names. Engine-written block bans spell the prefix out (`addr/64` for IPv6). Both the
  session port and the game port enforce an explicit `/128` as one exact address; every other entry refuses the address's block.
- The file is written atomically (temp file + rename) on every change; it is re-read on reload (spec 76 § 5). An unreadable file at start
  keeps an empty in-memory list, logs an error, and does not overwrite the file.
- Migration note (spec 12 § 4a): `format_version: 2` since ADR-4936. v1 → v2 rewrites every bare IPv6 `Address` entry to its `/128`
  form, which is what the entry meant under v1 (one exact address); the loader migrates in memory on read and `regia upgrade-project`
  rewrites the file. A later version keeps reading version 1 and 2.

### 7.4 Reconnect grace

- An unexpected drop of an `InGame` peer (`timeout`, `transport_error` or `suspended`) moves it to `Grace` for `session.reconnect_grace_s`
  (default 60 s; 0 disables). Its slot stays reserved; its controller and owned entities stay; `SessionRules::on_player_dropped` decides what
  happens to them meanwhile (default: the possessed character is frozen, takes no input, and stays relevant to others).
- A `Hello` with the matching `reconnect_token` within the grace resumes the same `PeerId` and players: the peer goes to `Loading` (the
  current level), then `Joining` with a join snapshot, and owns its entities again. `SessionRules::on_player_resumed` runs. The `Welcome`
  of a resume carries a new token; the old one is dead (ADR-1892).
- When the grace expires: `PlayerLeft { reason: timeout }`, then `Gone`.
- Deliberate leaves (`client_quit`) and kicks never enter `Grace`.
- **Replication games** (ADR-8298, spec 120 § 10.3 and § 8.5). A game hosted through `NetLink` (`HostedLink`, the `net-game`
  template) gets the same promise: a player whose transport connection times out is `Dormant` for `LinkOptions::resume_grace_ms`
  (default 60 s, the same as `session.reconnect_grace_s`; 0 disables) with its `PeerId`, entities, ownership and interest state kept and
  its baselines dropped; other clients see its entities with `NetInterrupted`. The client comes back with `NetLink::resume` (or
  `resume_on` for a non-UDP socket), presenting the resume ticket of its last accept in the join token (16 bytes, single use, rotated on
  every accept): it keeps its `PeerId` and owns its entities again, and the first frame it gets is the full state. A ticket presented
  while the server still holds the old connection takes it over at once (as the session's live-token takeover). When the grace ends
  first, the game's `NetGraceHook` (`on_grace_expired`) runs once and decides `Despawn` or `Keep`; then `LinkEvent::Left { timeout }`.
- **Live-token takeover** (ADR-8180, spec 120 § 10.3). The token also resumes a peer the server still holds as `Loading`, `Joining` or
  `InGame` (interrupted or not): a client that comes back before the server's own timeout fires (one-way loss, a game that retries at
  once, a restarted process) replaces its old link at once. Same `PeerId`, no new allocation and no capacity check (a returning player is
  never refused `server_full` by its own old slot), the old link is dropped without a goodbye (its streams fail with `link_replaced`),
  `PlayerResumed` fires and the peer goes to `Loading` as for a grace resume.
- **Same-address rejoin.** When the transport replaces a quiet sealed peer because a new handshake completed from its address (a crashed
  client restarted on the same port), the old link ends as `timeout`, so an `InGame` player enters `Grace` and its token resumes it.
- **Flow control is not a drop.** A reliable window that the transport refuses with backpressure (or a per-second rate cap) holds the
  session's messages for the next flush: streams to that peer pause (`SessionServer::set_link_backpressure`; `session.state` shows
  `backpressured`, true while the window is backed up or the peer is interrupted), `SessionServer::try_send_game` hands the bytes back, nothing is dropped. Only a held backlog past
  `MAX_DEFERRED_RELIABLE_BYTES` (8 MiB) or a refusal other than backpressure ends the link (`reliable_overflow`); an overflow while the peer
  is interrupted is the link's fault and enters `Grace`.
- **Rebinds.** When the transport moves an authenticated peer to a new address, the session core follows it: `peer_addr`, the peer rows
  and an address ban (`ban_player`) use the new address.
- **Resume transport.** `NetSession::resume` reconnects over the transport kind the session connected with: the `SocketOpener` of
  `connect_via` (the lobby uses it, so a Steam lobby client resumes over Steam sockets), or UDP after `connect` / a `connect_on` with no
  `set_socket_opener`.

## 8. Streams

For one-off large reliable data (a custom map file, a replay to download, player-made content): `NetSession::send_stream(to, kind: Name,
bytes)` sends up to 64 MiB in chunks over a reliable channel, paced to use at most `session.stream_share` (default 25 %) of the connection's
budget, so gameplay keeps flowing. The receiver gets `NetStreamProgress { id, received, total }` and `NetStreamComplete { id, kind, bytes }`,
or `NetStreamFailed { id, reason }` if the connection drops, with `stream_timeout` when the connection's incoming streams together move less than 256 bytes per second over
`session.stream_idle_timeout_s` (default 30), or with `stream_buffer_full` when the server's incoming streams pass
`session.max_stream_buffer_bytes` (default 512 MiB; ADR-1892). Chunks of a failed stream are dropped. At most 4 streams in flight per connection. Streams are not replicated state and are
not resent after reconnect.

## 9. Game hooks and events

- `GameRules` (spec 14, `regia_framework`) is not changed. Its existing `on_player_joined(ctx, player: Entity)` is called on the server
  when a remote player reaches `InGame` (with the `Player` entity spawned per spec 70 § 4), and `on_player_left(ctx, player)` when it
  reaches `Gone`; local players keep today's behavior.
- Network-only hooks live in a new trait in `regia_net` (so `regia_framework` does not depend on `regia_net`'s types):
  `pub trait SessionRules: Send + Sync + 'static` with default methods `on_player_joining(&mut self, hello: &Hello) -> JoinDecision`
  (accept, or refuse with a text sent as `kicked`; runs after the § 4 checks), `on_player_dropped(&mut self, world: &mut World,
  player: Entity)`, `on_player_resumed(..)`, `on_player_left_reason(.., reason: &DisconnectReason)`, `on_travel_finished(.., level)`,
  `allow_idle_slowdown(&self) -> bool` (default false) and `match_over(&self, world: &World) -> bool` (default false; polled by the dedicated host, spec 76 § 5). A game installs one with `app.set_session_rules(MyRules)`; without one the
  defaults apply.
- Events on the server: `PlayerJoined`, `PlayerLeft { player, reason }`, `PlayerDropped`, `PlayerResumed`, `TravelStarted`, `TravelFinished`.
  On clients: `SessionStateChanged { from, to }`, `Disconnected { reason, detail }`, `TravelStarted`, `TravelFinished`.

## 10. Developer tools

### 10.1 Network conditioner

- Available in dev builds on both sides, per direction (outbound, inbound): `latency_ms`, `jitter_ms` (uniform ± around the latency),
  `loss_pct`, `duplicate_pct`, `reorder_pct` (a chosen packet gets an extra delay of `reorder_extra_ms`, default 20, so later packets overtake it),
  `bandwidth_bytes_per_s` (0 = none, otherwise a token bucket that delays packets). Every random draw comes from a seeded RNG (`seed` field),
  so a conditioner run with the same traffic is repeatable.
- Presets: `lan` (1 ms), `good` (30 ms, 2 ms jitter, 0.1 % loss), `average` (60 ms, 8 ms, 1 %), `bad` (120 ms, 20 ms, 3 %, 1 % reorder),
  `terrible` (250 ms, 50 ms, 8 %, 2 % duplicate, 3 % reorder).
- Order of effects per packet: loss, then delay plus jitter, then reorder extra delay, then duplication (the copy takes its own delay draw), then the bandwidth bucket.
- The conditioner is compiled out of shipping builds. As built (ADR-7212): `LinkShaper` is `#[cfg(debug_assertions)]`, the
  loopback hub passes packets through unshaped without it, and `tests/shipping_no_link_shaper.rs` is the compile check (run with
  `--release`).

### 10.2 Stats overlay

- A debug overlay (dev builds, toggled with the existing debug-overlay key binding) shows per connection: state, RTT, jitter, loss in/out,
  bytes per second in/out, packets per second, prediction corrections per second (spec 73) and interpolation delay (spec 74).

### 10.3 Simulated clients

- `NetSession::add_simulated_clients(n, input: SimInput)` (dev builds, server side) creates `n` in-process clients over the loopback
  transport that join like real ones and play recorded or random inputs (`SimInput::Recording(path) | Random { seed } | Idle`). They render
  nothing. `remove_simulated_clients(n)` removes `n` of them (newest first); `clear_simulated_clients()` removes every one. Used for load tests of the send loop (spec 72 § 14).

### 10.4 Play-in-editor with several clients

- PIE gains a player count and a mode: `Offline`, `ListenServer + n clients`, `DedicatedServer + n clients`. The editor runs the server and
  clients as separate worlds in its own process over the loopback transport (fast iteration), or with "separate processes" checked, as
  `regia server --clients n` processes (spec 76 § 10). Each client gets its own viewport; the conditioner applies to all of them.

## 11. API shape

```rust
#[derive(Resource)]
pub struct NetSession { /* ... */ }

impl NetSession {
    pub fn host(&mut self, bind: SocketAddr, opts: HostOptions, dedicated: bool)
        -> Result<u64, SessionError>;                                                        // listen or dedicated server; the session id
    pub fn connect(&mut self, target: ConnectTarget, opts: ConnectOptions) -> Result<(), SessionError>; // address, join code
    pub fn disconnect(&mut self);
    pub fn state(&self) -> ClientSessionState;                                              // client
    pub fn peer_state(&self, peer: PeerId) -> Option<PeerState>;                            // server
    pub fn travel(&mut self, level: &str) -> Result<u32, SessionError>;                      // server; returns the travel id
    pub fn kick(&mut self, player: NetPlayerId, reason: &str) -> Result<(), SessionError>;   // server
    pub fn kick_peer(&mut self, peer: PeerId, reason: &str) -> Result<(), SessionError>;     // server
    pub fn ban(&mut self, kind: BanKind, value: &str, until: Option<u64>, reason: &str)
        -> Result<BanId, SessionError>;                                                      // server; saves bans.ron
    pub fn send_stream(&mut self, to: PeerId, kind: &str, bytes: Vec<u8>) -> Result<StreamId, SessionError>;
}
```

`SessionError`: `AlreadyInSession`, `NotServer`, `NotClient`, `PlayerUnknown`, `LevelNotFound`, `StreamTooLarge`, `TooManyStreams`,
`SimClientsLimit`, `BadBan(String)`, `Transport(String)`, `PasswordNeedsEncryption` (ADR-1901) and `KnownHosts(..)` (ADR-3283).

As built (ADR-5651): `travel` takes the level path (at most 255 bytes, the `Welcome` / `TravelBegin` field; an empty or longer
path is `LevelNotFound`) and returns the `u32` travel id; `ban` takes the entry's fields and stamps `added` from the session
clock; a stream's `kind` is a `&str`. `host` binds UDP and returns the session id (`host_on` takes any datagram
socket, `host_local` runs socketless for simulated clients and play-in-editor). `ConnectTarget::JoinCode` is not resolved by `NetSession::connect`: it
answers `Transport` ("resolve it through the relay first"), and `session.connect` answers `join_code_unsupported`
(ADR-1663 (18); still open, listed in ADR-5651).

## 12. Files and formats

- `config/net.ron` section `session` (spec 70 § 7):

```ron
session: (
    timeout_ms: 20_000,
    interrupt_ms: 1_500,
    start_grace_ms: 30_000,
    connect_timeout_s: 10.0,
    handshake_timeout_s: 10.0,
    loading_timeout_s: 120.0,
    joining_timeout_s: 30.0,
    auth_timeout_s: 10.0,
    max_handshaking: 32,
    reserved_slots: 0,
    reconnect_grace_s: 60.0,
    stream_share: 0.25,
    stream_idle_timeout_s: 30.0,
    max_stream_buffer_bytes: 536870912,
    dev_allow_content_mismatch: false,
),
```

- `bans.ron` (§ 7.3) and `net-manifest.ron` (§ 4.1): new formats, `format_version: 1`.
- All three are registered in `check-formats` and `upgrade-project` at the v0.5.0 freeze; until then additions carry serde defaults.

## 13. Determinism

- Every state transition happens at a fixed point of a tick (session systems run in `PreUpdate` after receive), so with the loopback hub's
  simulated clock the whole lifecycle (including timeouts, counted in ticks of simulated time) replays identically.
- Timeouts in real sessions use the engine's monotonic clock; tests never use the wall clock (the loopback hub advances simulated time).

## 14. Agent protocol surface

Guide page: `book/agents/net-session.md` (topic `net-session`; add its `book/COVERAGE.md` rows): the state tables, the handshake checks and what each refusal means to a
player, travel, reconnect, bans, streams, the conditioner presets, simulated clients, multi-client PIE, and every code in § 7.1.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `session.state` | read | — | client: `{ state, reason?, server, peer_id, level }`; server: per peer `{ peer, players, state, rtt_ms, loss }` |
| `session.host` | play | `{ mode: listen\|dedicated, port?, level? }` | `{ session_id }` |
| `session.connect` | play | `{ address \| join_code, password?, players? }` | `{ state }` |
| `session.disconnect` | play | — | `{ state: "disconnected" }` |
| `session.travel` | play (server) | `{ level }` | `{ travel_id }` |
| `session.manifest` | read | — | this process's content manifest |
| `net.conditioner_get` / `net.conditioner_apply` | read / play (dev builds) | `{ preset? , outbound?, inbound?, seed? }` | the active settings (ADR-2413: the spec follows the shipped names; `net.conditioner_set` is the endpoint conditioner of ADR-1600, not this one) |
| `net.sim_clients` | play (server, dev builds) | `{ action: add\|remove\|clear, count?, input?: idle\|random\|recording, seed?, recording? }` | `{ simulated: n }` |
| `net.player_stats` | read | — | per remote player (server) or for the link to the server (client): `{ ping_ms, jitter_ms, loss_to_player, loss_from_player? }` (ADR-2413) |
| `net.stats_overlay` | play | `{ visible? }` (absent flips it) | `{ visible, lines }`: the in-game overlay of § 10.2, off by default (ADR-2413) |
| `net.handoffs` | read | — | the recent hand-offs of a `Gone` peer's entities: `{ total, records: [{ entity, from, to, peer? }] }` (ADR-2413) |

Errors: the § 7.1 codes when a connect attempt ends in that reason, plus `already_in_session`, `net_not_server`,
`level_not_found`, `conditioner_unavailable` (shipping build), `preset_unknown`, `sim_clients_limit` (above 256). As built
(ADR-5651): also `net_not_running` (no `NetSession` resource), `join_code_unsupported`, `transport_error`,
`password_needs_encryption`, `sim_clients_unavailable` (shipping build), `recording_invalid` and the known-hosts codes of
ADR-3283; no method of this table is client-only, so `net_not_client` is not produced here (it is `SessionError::NotClient`'s
code for API callers). Agent errors use `F0002` with `data.reason` set to the lowercase code, as neighboring built modules do (Decision (ADR-1585)).

## 15. Tests and acceptance

Loopback hub with simulated time.

1. **Happy path.** Client goes `Connecting → Handshaking → Loading → Joining → InGame`; the server sees `PlayerJoined`; with 10 % loss the
   path still completes.
2. **Each refusal.** One test per handshake check (§ 4 order), each producing its reason and detail; the order is checked by a client that
   fails two checks (the first one wins). A `Hello` from a future protocol version whose body does not decode still gets `version_mismatch`
   (the prefix is enough).
3. **Content mismatch detail.** A client missing one DLC pak and with a different mod version gets exactly those two items; a different
   texture pak alone does not refuse the join.
4. **Timeouts.** Each state's timeout (simulated time) produces its reason; keepalives keep an idle `InGame` connection alive for 10 minutes.
5. **Join snapshot.** A level entity removed before join never appears on the client; the joining client is `InGame` once game state and its
   own entities arrived, while 300 other entities stream in over later ticks within the spawn budget.
6. **Travel.** Two clients travel; persistent controllers keep their `NetId`s; a slow client joins the new level later without blocking the
   fast one; a stale `LevelReady` is ignored; a travel during loading restarts loading.
7. **Disconnect reasons.** Every § 7.1 code is produced by some test and delivered to both sides where applicable.
8. **Bans.** Each kind matches; an expired entry does not; atomic save survives a simulated crash between write and rename (old file intact);
   an unreadable file is not overwritten.
9. **Reconnect.** A dropped client rejoins within the grace with the same `PeerId` and owns its character again; after the grace it is a new
   join with a new `PeerId`; a quit never enters `Grace`. A full server (every slot taken) still resumes a
   grace slot; a suspended client resumes; a ban added during the grace refuses the resume.
10. **Streams.** A 10 MiB stream completes at no more than 25 % of the connection budget while replication continues; a drop mid-stream
    reports `NetStreamFailed`; a 65 MiB stream is refused.
11. **Conditioner.** Each parameter measurably applies (latency in ticks, loss and duplicate rates within ±1 % over 100,000 packets); the same
    seed gives the same packet log; the conditioner is absent in a shipping build (compile check).
12. **Simulated clients.** 64 simulated clients join and play a recording against a server; `clear` removes them all, and `remove` takes the newest first.
13. **Agent.** Every method round-trips; each error code is produced by a test; `cargo xtask agent-coverage` passes.

## 16. Platform notes and performance budget

- Linux, Windows and Android clients; Linux and Windows dedicated servers (spec 76); Android listen servers (up to 8 clients). On Android,
  a suspended app keeps no connection alive: on `suspended` the client sends `suspended` if it can, and the server's grace period (§ 7.4)
  covers a quick return either way (a lost goodbye ends as `timeout`, which also enters grace). Resuming reconnects with the token if the process survived.
- Budgets on the reference classes of `06-testing-and-ci.md` § 3. Server (Desktop reference): handshake handling for 32 concurrent joins under 0.2 ms per tick (span `net_session`); content manifest comparison is constant time
  in the number of assets (hashes only).
- Client (Desktop reference): dev-build manifest computation for a project of 20,000 assets under 1 s (one pass over the dev cache index; logged).

## 17. Out of scope

Lobbies, matchmaking, invites and parties (D6, using `JoinRequested` from `regia_steam`); host migration; travel that keeps the old level
running while the new one loads; voice; spectator-only connections (a later spec; they would be a player kind without a character).

## 18. Provenance

- Set spec ids compared in review (ADR-1585): `runtime-networking-transport-debugging`, `runtime-networking-overview`. Changed in review to avoid tracking
  the set's packet-simulation and simulated-connection lists: reorder mechanism (extra delay instead of hold-until-next-send), effect order,
  simulated-client add/remove/clear verbs. Regia deliberately has no version fallback (the set's handshake falls back to older versions).
- Set spec ids used: `runtime-networking-transport-debugging` (handshake, version checks, close outcomes, packet simulation, simulated
  connections, flood tracking), `runtime-networking-overview` (behavior only, ADR-1526).
- Public sources: G. Fiedler, "Gaffer on Games": "Client Server Connection" (connection states, request/challenge, timeouts, keepalives) and
  the public netcode protocol standard written alongside it (connect tokens); Valve Developer Community wiki, multiplayer networking
  article (connection flow, level changes); RFC 4632 (CIDR notation) for address-range bans.
- Regia design choices: the state tables and timeouts, the strict no-fallback version rule, the gameplay-only content hash, the join snapshot
  order, per-peer travel completion, reconnect grace with tokens, streams with a budget share, the conditioner's seeded draws and presets.

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

## Addendum (ADR-7905): netcode next generation, 2026-11-15

- **§ 7.4 "owns its entities again" holds only for session-only games at v0.5.0**: replication games join over `NetLink` ->
  `ServerBridge`, which deletes the replication peer on `PeerLeft` (`bridge.rs:319-321` -> `runtime.rs:558-567`). Spec 120 § 10.3
  designs resume for every game kind (dormant replication peers, live-token takeover, slot reclaim, automatic reconnect).
  **Built (N216, ADR-8298):** dormant replication peers, the resume ticket in the `NetLink` handshake, live-token takeover and slot
  reclaim for replication games, the full state after a resume and the `on_grace_expired` hook (§ 7.4 "Replication games"). Automatic
  reconnect with backoff is still open (spec 120 § 10.3).
- **Timeouts**: `session.timeout_s` does not reach the transport configs at v0.5.0 (`net_session.rs:891-897`, :957-963). Spec 120
  § 10.1-10.2 sets one timeout policy (default 20 s, start grace 30 s, never for ping or loss) and an `Interrupted` state.
