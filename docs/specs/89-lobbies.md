# Spec 89 — Lobbies, matchmaking and the Steam sockets transport (`regia_net`, `regia_steam`)

Status: proposed (ADR-1877). Version line: v0.5.0 (`docs/versions.md`, `docs/master-plan.md` § 3.5b deliverables
"Steam Networking Sockets transport, Steam lobbies and matchmaking"; parity rows D5/D6). Builds on spec 77 (session
lifecycle), spec 76 (dedicated server) and spec 24 (Steam). No reviewed public spec exists for this item: the Regia
docs are the source; the Steam behavior described here follows Valve's public Steamworks documentation
(ISteamMatchmaking, ISteamNetworkingMessages, ISteamFriends) and is cited in `docs/provenance.md`.

## 1. Purpose

A game finds other players and starts a session without typing an address:

1. **Lobbies.** A small, platform-held room with key/value data, per-member data, a member limit, a visibility and
   a joinable flag. A lobby is not a game session: it is where players gather, agree and get told where the session
   is.
2. **Matchmaking.** A deterministic state machine over lobbies: list with filters, join the best match, widen the
   search in rounds, create a lobby when nothing fits, wait for ready, launch.
3. **Steam sockets transport.** A `DatagramSocket` over Steam's connectionless peer-to-peer messages, so the
   unchanged spec 77 session (handshake, reliability, travel, reconnect) runs over Steam's relay network with NAT
   traversal, addressed by Steam id instead of IP.

Two lobby backends ship: **local** (in-process, a shared in-memory directory: tests, play-in-editor with N clients,
LAN builds without a platform) and **Steam** (ISteamMatchmaking). Games code against one trait.

Not in scope (§ 12): voice chat (spec 59), parties that persist across sessions, skill rating, dedicated-server
fleet allocation, Steam game-server login, Google Play / console platforms, lobby UI widgets (parity row J3,
`regia_ui`).

## 2. Crates and files

| Where | What |
|---|---|
| `regia_net::lobby` (new module dir) | Types, limits, validation, `LobbyQuery` evaluation, `LobbyBackend` trait, `LocalLobbyHub` backend, `Matchmaker`, `Lobbies` resource and `LobbyPlugin` |
| `regia_net::socket` | `impl DatagramSocket for Box<dyn DatagramSocket + Send>` so a transport chosen at run time plugs into `NetSession::host_on` / `connect_on` |
| `regia_net::settings` | A `lobby` section in `config/net.ron` (§ 6) |
| `regia_steam::lobby` (new) | `steam_lobbies(&Steam) -> Arc<dyn LobbyBackend>` over the `SteamBackend` seam (`SteamBackend::lobbies()`: the real ISteamMatchmaking backend in `real_lobby.rs`, the mock's `LocalLobbyHub` member) and `SteamLobbyPlugin { members_only }` (ADR-5656: there is no `SteamLobbyBackend` type) |
| `regia_steam::sockets` (new) | `SteamSocket`: `DatagramSocket` over peer-to-peer messages; Steam id ↔ synthetic address mapping; `SteamLobbyTransport` |
| `regia_steam::mock_net` (new) | `MockSteamNet`: a shared multi-user mock (lobbies and messages) for tests without the SDK |
| `regia_steam::real` | The ISteamMatchmaking / ISteamNetworkingMessages calls (feature `steamworks`, desktop) |
| `regia_agent::net_lobby` (new) | `lobby.*` and `matchmaking.*` agent methods |

No new dependency. `regia_steam` gains a dependency on `regia_net` (no cycle: `regia_net` depends on neither
`regia_agent` nor `regia_steam`).

## 3. Types (Rust)

```rust
/// A lobby's id. Steam: the lobby's 64-bit Steam id. Local: hub-assigned, never 0.
pub struct LobbyId(pub u64);
/// A lobby member. Steam: the member's 64-bit Steam id. Local: the id given to `LocalLobbyHub::member`.
pub struct MemberId(pub u64);

pub enum LobbyVisibility { Public, FriendsOnly, Private, Invisible }

pub struct LobbySpec {                 // what `create` asks for
    pub visibility: LobbyVisibility,
    pub max_members: u32,              // 1..=MAX_LOBBY_MEMBERS (250)
    pub data: BTreeMap<String, String>,// written right after creation, before `Created` is reported
    pub joinable: bool,
}

pub struct LobbyMember { pub id: MemberId, pub name: String, pub data: BTreeMap<String, String> }

pub struct LobbyInfo {
    pub id: LobbyId,
    pub owner: MemberId,
    pub visibility: LobbyVisibility,
    pub max_members: u32,
    pub joinable: bool,
    pub members: Vec<LobbyMember>,     // join order; listings carry the count only (members empty)
    pub member_count: u32,
    pub data: BTreeMap<String, String>,
    pub server: Option<LobbyServer>,   // where the game session is, once launched
}

pub enum LobbyServer { Address(SocketAddr), Steam(u64) }

pub enum LobbyEvent {
    Created { request: RequestId, result: Result<LobbyId, LobbyError> },
    Entered { request: RequestId, result: Result<LobbyId, LobbyError> },
    List { request: RequestId, result: Result<Vec<LobbyInfo>, LobbyError> },
    MemberJoined { lobby: LobbyId, member: MemberId },
    MemberLeft { lobby: LobbyId, member: MemberId },
    DataChanged { lobby: LobbyId, member: Option<MemberId> },  // None = lobby data
    OwnerChanged { lobby: LobbyId, owner: MemberId },
    Chat { lobby: LobbyId, from: MemberId, text: String },
    ServerSet { lobby: LobbyId, server: LobbyServer },
    InviteAccepted { lobby: LobbyId, from: MemberId },       // Steam GameLobbyJoinRequested
    Left { lobby: LobbyId, reason: LeaveReason },             // we are no longer in it
}

pub trait LobbyBackend: Send + Sync {
    fn name(&self) -> &'static str;                           // "local" | "steam" | "none" (no platform)
    fn local_member(&self) -> Option<MemberId>;
    fn create(&self, spec: &LobbySpec) -> Result<RequestId, LobbyError>;
    fn join(&self, lobby: LobbyId) -> Result<RequestId, LobbyError>;
    fn leave(&self, lobby: LobbyId);
    fn request_list(&self, query: &LobbyQuery) -> Result<RequestId, LobbyError>;
    fn lobby(&self, lobby: LobbyId) -> Option<LobbyInfo>;     // a joined lobby, members included
    fn still_in(&self, lobby: LobbyId) -> bool { self.lobby(lobby).is_some() } // cheap membership (ADR-5217)
    fn set_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError>; // owner
    fn set_member_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError>;
    fn set_joinable(&self, lobby: LobbyId, joinable: bool) -> Result<(), LobbyError>;            // owner
    fn set_server(&self, lobby: LobbyId, server: LobbyServer) -> Result<(), LobbyError>;         // owner
    fn send_chat(&self, lobby: LobbyId, text: &str) -> Result<(), LobbyError>;
    fn poll(&self) -> Vec<LobbyEvent>;                        // drained once per frame
}
```

Operations that wait on the platform (`create`, `join`, `request_list`) return a `RequestId` immediately; the result
arrives as an event on a later `poll`. The local backend answers on the next `poll` (one frame), so tests see the
same ordering as Steam.

### 3.1 Limits and validation (shared by both backends)

| Limit | Value | Source |
|---|---|---|
| Members per lobby | 1..=250 | Steamworks documentation, `CreateLobby` |
| Key length | 1..=255 bytes, no NUL | Steamworks `k_nMaxLobbyKeyLength` (documented) |
| Value length | ≤ 8192 bytes, no NUL | Steamworks `SetLobbyData` documentation |
| Chat message | 1..=4096 bytes UTF-8, no NUL (empty refused) | Steamworks `SendLobbyChatMsg` documentation |
| Keys per lobby, and per member (Regia limit) | ≤ 64 (the engine keys count) | Regia: bounds the local backend and agent output |
| Results per list | 1..=50 (default 50) | Steamworks `AddRequestLobbyListResultCountFilter` |

Keys starting `regia.` are reserved for the engine (§ 4); `set_data` from a game with such a key is refused
(`reserved_key`) unless it goes through the engine helpers.

### 3.2 Queries

```rust
pub struct LobbyQuery {
    pub strings: Vec<StringFilter>,    // (key, value, Compare): lexicographic byte compare
    pub numbers: Vec<NumberFilter>,    // (key, i32, Compare): the value parsed as i32; unparsable never matches
    pub near: Vec<NearFilter>,         // (key, i32): sort, no filtering; earlier filters dominate
    pub open_slots: Option<u32>,       // at least this many free slots
    pub distance: Distance,            // Close | Default | Far | Worldwide (Steam region filter)
    pub max_results: u32,              // 1..=50
}
pub enum Compare { Eq, Ne, Lt, Le, Gt, Ge }
```

Semantics (the local backend's, and the expected Steam behavior): a lobby is listed when it is `Public`, joinable,
not full, and every string and number filter holds (a missing key fails every filter except `Ne`). Results are
sorted by the near filters (sum of `|value - target|` per filter, compared lexicographically in filter order; a
missing or unparsable key sorts last), then by member count descending, then by lobby id ascending, and truncated to
`max_results`. The local backend ignores `distance` (everything is "close"). Steam applies the filters on its
servers and returns its own order for ties; the matchmaker never depends on tie order for correctness.

## 4. Engine keys and the content handshake

On `Lobbies::create` the engine writes reserved keys from the lobby identity. `LobbyPlugin` takes the protocol and
content hash from the app's `NetSession` manifest (spec 77 § 4.1, the values the content handshake compares) at
`finish` (before `Startup`; a protocol hash `NetSessionPlugin` has not filled in yet is computed the same way) and
keeps them equal to it every frame, so mods mounted at session start move the filter with the handshake (ADR-7881,
which supersedes the "game's call only" part of ADR-5656). A game that calls `Lobbies::set_identity(game, &manifest)`
keeps its own identity (`LobbyIdentity::Explicit`); `Lobbies::set_game(game)` names the game and leaves the hashes to
the plugin. The game id is not in the manifest, so without a `set_identity` or `set_game` it is empty. The
`lobby_follow_manifest` system (ADR-7213) runs the same copy in `Startup` and can be called before an early create;
an owner whose lobby was created with stale hashes re-sends the engine identity keys on its next pump.

A `Lobbies` with no identity (no `NetSession` to take it from, nothing set by the game, an empty game id) refuses
`create`, `list` and `start_matchmaking` with `lobby_no_identity` (`LobbyError::NoIdentity`, logged), so a build that
forgot its identity never lists or joins every other build's lobbies on a shared app id. `Lobbies::allow_unidentified
(true)` opts in to the old behavior (empty game id, zero hashes). A backend with no platform still answers
`lobby_unavailable` first.

| Key | Value |
|---|---|
| `regia.v` | `2` (lobby key layout version; `1` before ADR-8897) |
| `regia.game` | the game id (`NetGame::GAME_ID` of the session, or the project name) |
| `regia.protocol` | `protocol_hash` as 16 lowercase hex digits |
| `regia.content` | `content_hash` as 16 lowercase hex digits |
| `regia.server` | the launched session's address: `udp:<ip:port>` or `steam:<steam id>` |
| `regia.state` | `open` → `launching` → `in_game` |
| `regia.mode` | optional (ADR-8897): the session's mode, the current playlist row's `mode` (rule-set id or mode string), else the board's mode lobby id |
| `regia.playlist` | optional (ADR-8897): the session's playlist id |

`regia.state` moves to `in_game` when the owner's session shows play: a hosted session with a peer in game (or the
owner alone in the lobby), or the owner's own client in game on a session hosted elsewhere. An owner who publishes a
session it never joins stays at `launching`: the engine cannot see that session.

Member data key `regia.ready` is `1` when the member is ready.

A host publishes `regia.mode` and `regia.playlist` with `Lobbies::publish_session(&session)` (or
`Lobbies::set_rotation_keys`): a lobby created afterwards carries them and the owner of the current lobby writes
them at once. A browser or matchmaker filters on them with `LobbyQuery::with_mode` and
`LobbyQuery::with_playlist` (ordinary string filters, also reachable as `strings` over the agent protocol).

`Lobbies::list` and the matchmaker add `regia.game == <game>`, `regia.protocol == <hash>` and (unless
`lobby.match_content` is false) `regia.content == <hash>` to every query, so a build never matches a lobby it could
not join. The session's own handshake still checks the full manifest (spec 77): the lobby filter only saves the
round trip.

## 5. Matchmaking

`Matchmaker` is a pure state machine stepped once per frame with the backend's events. No wall clock: deadlines are
counted in steps (frames), so a test drives it with fixed steps.

```text
Idle --start--> Listing{round} --List(ok)--> Joining{candidates}
Listing --List(empty or err)--> Listing{round+1} (distance widened) | Creating (rounds exhausted, create_if_none)
                                                                    | Failed(no_match)
Joining --Entered(ok)--> InLobby{owner: false}
Joining --Entered(err)--> next candidate | Listing{round+1} (candidates exhausted)
Creating --Created(ok)--> InLobby{owner: true}       Creating --Created(err)--> Failed(create_failed)
InLobby --ServerSet--> Launched{server}               InLobby --Left--> Failed(left)
any --cancel--> Idle (leaves the lobby it joined or created)
```

- Rounds widen the distance: `Close`, `Default`, `Far`, `Worldwide` (configurable ladder, § 6).
- Candidates are taken in list order; a full or vanished lobby (join refused) moves to the next one.
- `step_timeout` steps without the awaited event give up on the current request, so a lost platform callback never
  wedges matchmaking: a list counts as an empty round; a join leaves the lobby, is abandoned and the next candidate
  is tried (the next round when none is left); a create is abandoned and matchmaking fails with `request_timeout`.
- `Launched --Left--> Failed(left)`, as from `InLobby`. A join into a lobby that already has a server (one kept
  joinable with `lobby.joinable_in_game`) goes straight to `Launched`.
- `Lobbies::start_matchmaking` (`matchmaking.start`) checks the create spec with the engine keys added when it
  starts, when `create_if_none` (`members_invalid`, `too_many_keys`, `key_invalid`, `value_too_long`), as `create`
  does, so a bad spec is not reported rounds later as `create_failed` (ADR-5656). A join-only request
  (`create_if_none: false`) never sends the spec, so its member limit and key count are not checked; its game keys
  are still checked one by one (`key_invalid`, `value_too_long`, `reserved_key`), as before.
- **Abandoned requests.** A create or join given up on (cancel, `step_timeout`, or `Lobbies::leave` while a plain
  create or join is in flight) is remembered (at most 32, oldest forgotten); when its `Created` / `Entered` arrives
  later, the lobby is left at once, so no orphan lobby with us in it stays listed. `Lobbies` settles them every
  frame, matchmaking or not.
- **Ready and launch.** In `InLobby` the local member sets `regia.ready`. The owner's matchmaker reports
  `ready_to_launch` when at least `min_players` members are in and all are ready. Launch is the game's call
  (`Lobbies::launch(server)`): it sets `regia.server`, the lobby's game-server field, `regia.state = launching` and
  `joinable = false` (unless `lobby.joinable_in_game`). Every member's matchmaker then reports `Launched { server }`
  and, with `lobby.auto_connect`, `LobbyPlugin` connects the `NetSession` (spec 77) to it. `Lobbies::launch_hosting`
  drops the session it just hosted if the launch cannot be published.
- **A lobby that vanishes.** When the backend no longer has us in the current lobby and no `Left` event came, the
  result is the same as `Left`: the lobby's connect state is cleared and matchmaking fails with `left`.
- **Cancel** applies to matchmaking only: `matchmaking.cancel` leaves the lobby matchmaking joined or created, and
  never one joined by hand.

Same inputs and event order give the same transitions and the same chosen lobby: the state machine has no
randomness and no clock.

## 6. Settings: the `lobby` section of `config/net.ron`

`config/net.ron` is `format_version` 2 with a v1 → v2 migrator (ADR-4854) and is registered in `cargo xtask
check-formats` (ADR-2316), `LobbySettings` included. Every `lobby` field has a serde default, so files without the
section load with the defaults below.

```ron
lobby: (
    max_members: 8,
    visibility: Public,
    min_players: 2,
    match_content: true,
    auto_connect: true,
    joinable_in_game: false,
    rounds: [Close, Default, Far, Worldwide],
    results_per_round: 50,
    step_timeout: 600,
    create_if_none: true,
    steam_channel: 0,
),
```

## 7. Steam sockets transport

`SteamSocket` implements `regia_net::DatagramSocket` over Steam's connectionless peer-to-peer messages
(ISteamNetworkingMessages: `SendMessageToUser` / `ReceiveMessagesOnChannel`, sessions accepted through
`SteamNetworkingMessagesSessionRequest_t`). Every datagram is sent **unreliable, no Nagle**: `regia_net` already does
reliability, ordering, fragmentation and congestion control (spec 77), and Steam's relay network provides NAT
traversal and IP hiding.

**Addresses.** The session layer is keyed by `SocketAddr`. A Steam id maps to a synthetic IPv6 address in the
unique-local range (RFC 4193): `fd52:6567:6961:0:<steam id as four 16-bit groups>` (`52 65 67 69 61` = "Regia"),
port = the messages channel. `steam_peer_addr(SteamId, channel)` and `steam_id_of(SocketAddr)` are exact inverses;
an address outside the prefix is not a Steam peer and `send_to` refuses it (`InvalidInput`). The local address is
the local user's Steam id mapped the same way. Synthetic addresses never reach the operating system.

**Accept policy.** `SteamAccept::Anyone` (default; the spec 77 handshake authenticates) or `SteamAccept::Listed`,
an allow-list the lobby plugin keeps equal to the current lobby's members.

**Limits.** Datagrams up to `MAX_DATAGRAM_BYTES` (2048) are sent; longer sends are refused (`InvalidInput`), longer
receives are truncated as with UDP. A `recv_from` that finds the socket's internal queue empty takes at most
`RECEIVE_BATCH` (64) messages in one platform call, so the socket never holds more than one batch; the rest wait in
Steam's own per-session queue. `SteamSocketStats` counts sends, receives, platform receive calls and the sends
Steam refused (`send_errors`); oversize and non-Steam sends return `InvalidInput` before reaching Steam and are not
counted.

**Raw datagrams.** A game with its own netcode uses `SteamSocket` directly (spec 24 § 2 "raw datagram API").

## 8. ECS: `Lobbies` and `LobbyPlugin`

`Lobbies` (resource) holds the backend (`Arc<dyn LobbyBackend>`), the current lobby, the matchmaker, the last list
result, a bounded buffer of recent events (256, oldest dropped) and an optional transport factory
(`LobbyTransport`: opens the datagram socket and maps a member to its peer address; the Steam plugin installs the
Steam one). `LobbyPlugin` adds `LobbyMessage` (one per `LobbyEvent`) and a `PreUpdate` system that polls the backend,
steps the matchmaker, keeps the current lobby snapshot fresh and, with `auto_connect`, connects the `NetSession`.
The system has a `tracing` span (`lobby_pump`).

## 9. Agent protocol (`book/agents/net-lobbies.md`)

| Method | Params | Result |
|---|---|---|
| `lobby.status` | — | `{ backend, transport, local_member, lobby?, owner, connected_to?, connect_error?, matchmaking, last_list, recent_events, identity }` (`identity`: `{ source: session \| explicit \| unset, game, protocol, content, allow_unidentified }`, ADR-7881) (`connected_to` only after a successful auto-connect; `connect_error` after a failed one, which is not retried in the same lobby) |
| `lobby.list` | `strings?, numbers?, near?, open_slots?, distance?, max_results?` | `{ request }`; the result lands in `lobby.status` `last_list` |
| `lobby.create` | `visibility?, max_members?, data?` | `{ request }` |
| `lobby.join` | `lobby` | `{ request }` |
| `lobby.leave` | — | `{ left }` |
| `lobby.set_data` | `key, value?` (null deletes), `member?: bool` | `{ ok }` |
| `lobby.ready` | `ready: bool` | `{ ready }` |
| `lobby.chat` | `text` | `{ ok }` |
| `lobby.launch` | `address?`, `level?` (default: host a listen session through the lobby transport) | `{ server }` |
| `matchmaking.start` | `strings?, numbers?, near?, min_players?, create_if_none?, visibility?, max_members?, data?` | `{ state }` |
| `matchmaking.cancel` | — | `{ state }` |
| `matchmaking.status` | — | `{ state, round, candidates, lobby?, ready_to_launch, server?, error? }` |

Lobby and member ids are decimal strings in results (Steam ids exceed 2^53) and are read from a string or an integer.
Errors are `F0002` with `data.reason`: `lobby_not_running`, `lobby_unavailable`, `not_in_lobby`, `already_in_lobby`,
`not_owner`, `lobby_full`, `lobby_not_found`, `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`,
`chat_invalid`, `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`, `matchmaking_busy`, `no_transport`,
`session_error`, `net_not_running` (`lobby.launch` without a `NetSession`). Lobby event
codes (`lobby.status` `recent_events[].kind`): `created`, `entered`, `list`, `member_joined`, `member_left`,
`data_changed`, `owner_changed`, `chat`, `server_set`, `invite_accepted`, `left`. Matchmaking states: `idle`,
`listing`, `joining`, `creating`, `in_lobby`, `launched`, `failed`; failure codes `no_match`, `create_failed`,
`request_timeout`, `left`, `unavailable`.

## 10. Determinism

Lobby state is not simulation state: nothing here enters a replay or the authoritative tick. The local backend and
the matchmaker are deterministic (ordered maps, no clock, ids allocated in call order), so the same calls in the
same order give the same events. Steam's list order and callback timing are not deterministic; the matchmaker's
outcome then depends only on the order events arrive.

## 11. Tests and acceptance

Unit (`regia_net::lobby`): validation limits; query filters (each `Compare` on strings and numbers, missing keys,
open slots, visibility, joinable, full), near sort and tie-breaks; local backend create/join/leave/owner hand-off
(the earliest remaining member becomes owner), member data, chat, server set, list one frame late; matchmaker:
join best, skip full, widen rounds, create when none, fail when none and `create_if_none` false, timeout by steps,
cancel leaves, ready-to-launch, launched.
Integration (`regia_net/tests/lobby.rs`): two `NetSession`s matched through a `LocalLobbyHub`, the owner launches on a
loopback socket, the member auto-connects through `LobbyPlugin` and reaches `in_game` (fixed steps, manual clock).
`regia_steam/tests/lobby_sockets.rs`: `MockSteamNet` lobbies through `SteamLobbyBackend`; a spec 77 session hosted
on a `SteamSocket` and joined from another mock user reaches `in_game`; address mapping round trip; accept policy.
`regia_steam/tests/net_impaired_steam.rs` (ADR-8351, spec 120 § 10.8): the mock's seeded impaired links (`MockLink`,
the soak harness's profiles), and lobby-launched sessions on good-wifi, bad-wifi (5 min, no disconnect) and flappy, and
a Steam resume on bad-wifi with the same `PeerId`.
Agent (`regia_agent`): every method above through the dispatcher on a world with a local hub, error codes.
`regia_steam/tests/real_client.rs` (feature `steamworks`, skipped without Steam): create a lobby, read it back,
leave. **Owner/hardware acceptance (not automatable here):** two Steam accounts on two machines on different networks
find each other through `matchmaking.start`, launch, and play a template match over `SteamSocket` with the
conditioner on (master plan v0.5.0 definition of done).

## 12. Platforms

- **Linux, Windows:** both backends. Steam needs the `steamworks` feature and a running client.
- **Android aarch64:** the local backend and the matchmaker build and run; `regia_steam` compiles to the no-op
  (`lobby_unavailable`), as for every Steam call.
- **macOS (testing only):** as Linux.

## 13. Performance budget

`lobby_pump` per frame: ≤ 0.05 ms with a joined lobby of 16 members and no events (one poll, one snapshot diff);
`SteamSocket::recv_from` costs one platform call per 64 datagrams. Query evaluation in the local backend is
O(lobbies × filters + n log n) and only runs on `request_list`. No GPU work; no renderer involvement.

## 14. Out of scope

Voice chat; persistent parties and friends-list presence beyond `JoinRequested` / invites; skill or rating based
matchmaking (games add a number filter or near filter on their own key); server browsers for dedicated servers
(ISteamMatchmakingServers); Steam game-server login (spec 76 follow-up); host migration; console and mobile platform
lobbies; lobby UI widgets.

## 15. Self-hosted lobby hub (ADR-8559, NL-04)

`regia_net::lobby::hub` is a `LobbyBackend` with no platform behind it: a hub process anyone can run.

- **Pieces.** `HubServer` is the state core (sessions, presence, lobbies, the directory, invites, chat). It is pure and
  bounded: no socket, no clock, every call takes `now_ms`. `HubHost` runs it behind a `NetServer` that always seals
  (`CryptoMode::Required`) under the hub's `ServerIdentity`. `HubLobbyBackend` is the client: a `NetClient` pinned to the
  hub's `ServerKey`. `connect` refuses no key (`hub_key_unpinned`); a hub under another key ends the link
  (`hub_key_mismatch`) and no hub session is allocated.
- **Sessions.** A hub session is allocated on the client's `Hello`, which only arrives over a finished handshake. One session
  per connection; handles are unique case-insensitively. A session silent for `idle_ms` (30 s) expires; the client
  heartbeats every third of it.
- **Session addresses.** The owner confirms the game session (`set_server`, or `set_data` of `regia.server`): only then does
  the hub hold an address, and it sends it to the lobby's members only (`ServerSet`, the room snapshot). Directory cards drop
  `regia.server`; presence rows name a lobby only when it is public; invite cards carry a lobby id and a token, never an
  address.
- **Visibility.** Public: listed, anyone joins. Invisible: not listed, anyone with the id joins. Friends-only: not listed;
  a session with a proven player key that the owner lists (`set_friends`, by key) or an invite was bound to; no key, no
  entry (ADR-8633). Private: not listed; an invitee whose token matches (bound to the invitee,
  spent on use, expires after `invite_ttl_ms`, 2 h).
- **Directory.** A directory generation (listed lobbies) and a presence generation (sessions) move only when a row changes.
  A client's cursor gets a delta when the change logs (`changelog`, 128) cover it, a snapshot (`reset`) otherwise, nothing
  when current; pages stay near 32 KiB. `request_list` evaluates the `LobbyQuery` on the client's mirror after the hub's
  `ListDone`.
- **Caps** (`HubSettings`, defaults = DO27's): handle 24 bytes, chat 160 characters and 4 lines/s per scope, history 100,
  256 lobbies, 1024 sessions, 64 friends, invite TTL 2 h; Regia's own: 64 members, lobby data 8 KiB, member data 256 B,
  4096 invites, 1024 queued events per session (a slower reader is dropped).
- **Wire.** The `regia.lobby_hub` game (`HUB_WIRE_VERSION` 1) on the reliable channel, a bounded hand-written codec. The
  version is in the protocol id, so § 12.1 of spec 120 applies: no cross-version path, a clear `protocol_mismatch`.
- **Agent.** `lobby.hub_status` (`book/agents/net-lobby-hub.md`); fuzz target `net.lobby_hub_msg`.
- **Later rows.** NL-05: join brokering (port map, punch, relay) and transport-pinned player identity. NL-06: the
  `regia-lobby-hub` binary in `regia package --server`.
- **Join brokering and player identity (NL-05, ADR-8607).** Built. A session is bound to a `PlayerKey`, proven in the
  `Hello` by a Noise `X` message bound to the hub link's handshake hash (the transport is `NX`, so the client has no static
  key there); a mismatching `Hello` or `Welcome` is refused `hub_identity_mismatch`; one key holds one handle
  (`hub_identity_in_use`), a reconnect under the same key gets its session back, and a mute holds on the key. The owner
  offers its game socket's reach (`ReachOffer`), a member asks, the owner approves, and each side gets a `JoinPlan` with the
  other's candidates, in the order mapped, punch (relay fallback at 1.5 s), relay (at once when the RFC 4787 model says no
  pair works); `HubReach` runs it on the member's game socket and the path is reported (`LobbyEvent::Reached`,
  `lobby.hub_join_status`, `book/agents/net-lobby-hub-join.md`). Wire version 1 (ADR-8633: the proof is optional, version
  1 was unreleased).
- **Review fixes (ADR-8633).** The owner leaving withdraws the session address it confirmed (members get `ServerSet` of
  `None` and leave `in_game`); only the new owner confirms the next, under the lobby-data checks. An outbox overflow is
  repaired at the next drain by a resync (room, invites, open asks, directory snapshot); `HubHost` drains before it expires,
  keeps what the link refuses for the next update, and drops only a session that cannot drain. Writes (lobby data, member
  data, address, joinable) are limited to `data_per_sec` 20 per session and `lobby_data_per_sec` 160 per lobby
  (`hub_data_rate_limited`); global chat to `global_chat_per_sec` 16 hub-wide. A cursor rewind is taken once per
  `list_rewind_ms` (10 s), else ignored (`hub_list_rewind`). `HubHost` sends at most `reliable_msgs_per_s` 1024 reliable
  messages per peer per second. `Hello`'s player proof is optional; friends are player keys; an invite to a keyed session is
  bound to the key. Invite eviction takes from the lobby holding the most; a dormant link's session is held and resynced on
  resume.
- **Packaged hub process (NL-06, ADR-8648).** Built. `regia-lobby-hub` loads `hub.toml` (port, a server key
  created on first run outside any project, caps, per-address and per-identity rate limits) and serves until
  SIGTERM or SIGINT, then drains. `GET /health` and `lobby.hub_status` read the same report. Every `regia
  package --server` stages the binary, a default `hub.toml`, a README section and `deploy/regia-hub.service`,
  which starts it (ADR-8891). The address and
  identity limits sit in front of the core, including its member-data limits.

**Built (v07-net-ki-hosting-lobby, ADR-10069, 2026-11-20; KI-NT-045, -046).** `join` of a private or friends-only lobby the
caller may not enter answers `lobby_not_found`, as a missing id does (lobby ids are sequential, so distinct codes let
anyone probe which private lobbies exist); the hub's refusal counter keeps `hub_invite_only` and `hub_friends_only`. A
chat line dropped at a member's outbox cap is replayed by the resync from the first dropped line on (kept lines only, in
order, once), and a join plan dropped at the cap is sent again by the resync, to the member and to the owner.
