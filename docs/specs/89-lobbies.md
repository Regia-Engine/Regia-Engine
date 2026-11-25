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
lobbies; lobby UI widgets. The Steam dedicated-server browser stays out. A hub directory row that only adds a
listen-or-dedicated kind, with no address, is § 16.

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

## 16. Delegated sessions, relay-only reach, and the ECS-free join surface (ADR-10375)

NL-06 is the packaged hub process (ADR-8648), already built. The rows below continue the lobby series in this
spec. Spec 10's "v0.6 NL-07" is peer logging (ADR-8149) and is not one of these rows. v0.7's `HUB_WIRE_VERSION`
is 1 (`crates/regia_net/src/lobby/hub/wire.rs`). These rows bump it to 2. A version-1 peer is refused at the
handshake with `protocol_mismatch` (spec 120 § 12.1). No cross-version decode.

The hub still never carries game datagrams. A datagram whose first bytes are the session magic (`RGN1`) or the
relay magic (`RGNR`), received on the hub socket, is dropped. Game datagrams move only on the members' game
sockets, or through `RelayServer` when a plan's path is the relay. Acceptance is not a clean loopback: the
join tests drive `SimNat` and a `TestClock` (`now_ms` on the sans-IO core). No test sleeps or asserts on wall
time.

### 16.1 Current state at the v0.7 freeze (what is reused, what is absent)

Reused, not duplicated: `HubServer`, `HubHost`, `HubLobbyBackend`, invites and presence (NL-04, ADR-8559);
`ReachOffer`, `AskReach`, owner approval, `broker_reach`, `JoinPlan`, `ReachDone` (NL-05, ADR-8607); `nat`;
`relay`; `crypto` (`PlayerKey` is X25519, proven by one Noise `X` message); `server_host` (dedicated runtime,
`bans.ron`, password gate); `ticket::JoinTicketCheck`.

(a) A standalone ownerless `server_host` does not register a public lobby. `crates/regia_net/src/server_host/`
has no `HubServer`, `HubHost`, `HubLobbyBackend` or `LobbySpec` use. `regia-lobby-hub`
(`crates/regia_cli/src/bin/regia-lobby-hub.rs`) runs `HubProcess` only. Any hub client may call `create` today,
but the dedicated runtime never does. NL-07 adds that registration.

(b) Offering only a relay does not give relay-only hosting in either direction. `broker_reach`
(`lobby/hub/join.rs`) returns `Mapped` when a mapped candidate exists, `Relay` only when both NAT classes are
known and `predict` is `NeedsRelay` and a relay and join code are set, and otherwise `Punch` whenever a relay
or any candidate is present. `HubServer::approve` then copies the owner's candidates into the member's
`JoinPlan` and the member's candidates into the owner's `JoinPlan` (`lobby/hub/server.rs`, `approve_inner`).
`JoinPath` gathers local candidates and, unless `relay_first`, asks the relay to introduce both sides
(`server_host/relay_path.rs`). An empty candidate list on the offer therefore still plans a punch, and the
other side's candidates still travel. NL-08 adds an explicit flag.

(c) Addresses leak past the directory, into member lobby data. `public_data` drops `regia.server` from
directory cards (`lobby/hub/mod.rs`). `PresenceRow` has no address field. `InviteCard` has no address field.
`confirm_session` stores `LobbyServer` on the room and pushes `HubEvent::ServerSet` with that address to
every member (`lobby/hub/server.rs`); `room_state` copies `room.server` and the data map, which holds
`regia.server`. The test `the_session_address_goes_to_members_only_and_moves_presence` asserts both: members
receive the address, an outsider's events do not contain those bytes, and the card omits `regia.server`.
There is no per-`AskReach` limit and no hub ban list. `AdmitGate` limits every request to 30 per second per
address and 20 per second per identity (`lobby/hub/config.rs`). Those stay. NL-09 adds the tighter AskReach
windows, the in-flight cap, the ban check, and removes addresses from lobby data.

(d) ECS gates, and why:

| Piece | ECS-gated? | Where | Why |
|---|---|---|---|
| `relay`, `nat`, `crypto`, `ticket::JoinTicketCheck` | No | `lib.rs` (`pub mod relay`, `pub mod nat`, `pub mod crypto`; `pub use ticket::…`) | Core. `JoinTicketCheck` is the store hook, not `server_host::JoinTicket`. |
| `HubServer`, `HubHost`, `HubLobbyBackend`, `ReachOffer`, `JoinPlan`, `broker_reach` | No | `lobby/hub/mod.rs` re-exports them with no `cfg` | The hub core is sans-IO and has no ECS import. |
| `HubReach` | Yes | `lobby/hub/mod.rs` `#[cfg(feature = "ecs")] mod reach` | `reach.rs` calls `server_host::{JoinPath, JoinRoute, JoinSocket}`. |
| `server_host` (the module) | Yes | `lib.rs` `#[cfg(feature = "ecs")] pub mod server_host` | `server_host/mod.rs` imports `regia_app::App` and `regia_ecs` at the top, so every file in the module is compiled only with `ecs`. |
| `relay_split`, `JoinPath`, `JoinSocket` | Yes, only because of that module gate | `server_host/relay_path.rs` | The file uses `DatagramSocket` and does not import the ECS, `regia_app`, or `link`. |
| `JoinTicket` | Yes, same module gate | `server_host/join.rs` | The ticket type itself is bytes and a `SocketAddr`. The same file names `crate::link::LinkSocket`. |
| `RelayRegistrar` | Yes, two reasons | `server_host/join.rs` | The module gate, and the socket field is `LinkSocket`. `LinkSocket` is `Box<dyn DatagramSocket + Send>` (`link.rs`), and `pub mod link` is `#[cfg(feature = "ecs")]` because the link is App glue. |
| Running a `JoinPlan` on a caller-owned socket | Yes | `HubReach::new` | That is the only executor, and it is behind `ecs`. The plan type itself is core. |

(e) `HubHost` and `RelayServer` are not runnable together as a deployable process. Tests construct both in
one process (`tests/lobby_hub_join.rs`). The shipped binary does not construct a `RelayServer`. NL-11 adds
that process. Remote admin stays on the dedicated host (`server_host`, `[admin] token_file`) and is named
out in § 16.6.

### 16.2 Wire version 2 and bounds

`HUB_WIRE_VERSION` becomes 2. Every new list and string is bounded before the decoder allocates. A count
above its cap is `WireError` and allocates nothing. Fixed-size blobs are read into a stack array of that
size. Timers on the wire are `u32` milliseconds, never a length-prefixed blob. Caps below are the wire caps;
`HubSettings::sanitized` may clamp a configured value lower and must not raise it above the wire cap.

| Field | Cap | Read before allocation |
|---|---|---|
| Binding proofs on one `RegisterServer` | 8 (`MAX_BINDING_OWNERS`) | `u8` count; reject `> 8`, then `Vec` of that count |
| One binding proof | 145 bytes (`BINDING_PROOF_BYTES`) | exact length, stack buffer; not a `u16` length |
| Binding payload inside the proof | 49 bytes | fixed layout, no string |
| Game version | 32 bytes | `str_u8(32)` |
| Ban keys, one lobby | 64 | `u8` count; reject `> 64` |
| Ban keys, one server presence | 256 | `u16` count; reject `> 256` |
| Hub operator ban file | 4096 lines, 64 bytes each | line cap before the line is stored |
| Revocation set | 4096 entries | insert refused at the cap (`hub_revoke_full`); never evict a live revocation |
| Grace | `0` or `1..=86_400_000` ms | `u32`; a larger value is clamped to 24 h at `RegisterServer` |
| Expiry | `0` (none) or `<= now + 366 days` | `u64` in the fixed payload; a farther expiry is `hub_binding_invalid` |
| Candidates | `NAT_MAX_CANDIDATES` (8), existing | existing `decode_candidates` |
| Ticket | `MAX_TICKET_BYTES` (128), existing | existing `bytes_u16` |
| Join code | `JOIN_CODE_LEN` (6) after normalize, existing | existing |
| In-flight joins stored | 2 per player key, 64 on the hub | counter check before `joins.insert` |
| `AskReach` windows | 4 per player key per 10 s; 16 per source address per 10 s | timestamp deques, one stamp per admitted ask, no per-ask allocation beyond the stamp |

`ReachOffer` gains `relay_only: bool` (wire `u8`, only `0` or `1`; any other byte is `hub reach flag`).
Directory cards and room snapshots gain `kind` (`0` listen, `1` dedicated) and `relay_only` (`0` or `1`)
and, for a dedicated card, `capacity` and `load` as `u16`. They do not gain an address. `RoomState.server`
and `HubEvent::ServerSet { server }` leave the version-2 layout. A confirmed session is
`HubEvent::SessionConfirmed { lobby, confirmed: bool }` with no address. `LobbyInfo.server` from
`HubLobbyBackend` is always `None`. The local (in-process) backend is unchanged.

New requests, all on the reliable hub channel, same `HubMessage` envelope, trailing bytes still an error:

- `RegisterServer { server_proof, proofs, capacity, load, protocol, game_version, relay_only, grace_ms }`.
  `server_proof` is one Noise `X` message of `PLAYER_PROOF_BYTES` (128), prologue
  `regia.lobby_hub.server_presence v1`, payload the hub link's handshake hash. Same length check as a player
  proof, before `snow` runs.
- `RevokeBinding { server_key, serial }`.
- `CreateDelegated { spec, server_key }`. `spec` uses the existing lobby-spec bounds. A `regia.server`
  value in `spec.data` is `hub_address_in_data` and the lobby is not created.
- `SetReachMode { lobby, relay_only }`.
- `SetBans { lobby, keys }`. Owner only. Replaces that lobby's ban set.
- `CloseLobby { lobby }`. Owner of a delegated lobby, or the server session of an ownerless lobby.
- Existing `Offer`, `AskReach`, `Approve`, `ReachDone`, with the new `relay_only` byte on each offer.

New events:

- `ServerOnline` / `ServerOffline` to each bound owner of that server, and to the server session. Fields:
  server key, capacity, load, protocol, game version, relay-only, grace, owner count. No address.
- `LobbyAssigned { lobby, owner }` to the server session when a delegated lobby names it.
- `GraceStarted { lobby, deadline_ms }` and `GraceEnded { lobby }` to the members and the server.
  `deadline_ms` is `0` when grace is indefinite.
- `BindingChanged { server_key, serial, state }` to the owner who presented or revoked it (`live`,
  `expired`, `revoked`).
- Directory pages gain a server-card list, count-capped with `MAX_PAGE_ROWS` before allocation. A server
  card is: server key, capacity, load, protocol, game version, relay-only, owner count. No address, and no
  owner-key list (owner keys are not a directory field).

`public_data` still drops `regia.server`. In addition, `set_data`, `set_member_data` and create refuse a value that
is an address spelling, with `hub_address_in_data`. The spelling is a `SocketAddr` (every form that parser
accepts), a bracketed IPv4 or IPv6 with a port (a zone is split off, so a named zone counts), `udp:` or
`udp://` in any ASCII case plus one of those, or `steam:` in any ASCII case plus digits. A bare address with
no port, a hostname and a port alone are not a spelling. A handle that is a spelling is not stored: the
session opens under a stand-in that is not an address, so a presence row, a directory card, a room member
name and an invite do not carry it.
Free-text chat is not a listing and is not scanned. The acceptance scan covers directory pages, lobby
queries, presence events, room snapshots, `LobbyInfo` and invite cards.

### 16.3 NL-07 — delegated sessions and server presence

**Binding.** `PlayerKey` stays the X25519 key NL-05 already proves. This row does not add an Ed25519 key.
A binding is one Noise `X` message (`Noise_X_25519_ChaChaPoly_BLAKE2s`, Noise revision 34 § 7.4) to the
hub's `ServerKey`, prologue `regia.lobby_hub.server_binding v1`, made with `snow` the same way
`PlayerIdentity::prove` is. The payload is fixed:

`version u8 = 1 || server_key [32] || expiry_ms u64 || serial u64`

`expiry_ms == 0` means no expiry. Any other expiry must be `<= now_ms + 366 days` at verification or the
proof is `hub_binding_invalid`. `serial` is the owner's revocation id for this proof. The ciphertext is
readable only by the hub static key it was made for, so a proof made for another hub fails, and a low-order
static or ephemeral key fails `hub_binding_invalid` under the same all-zero X25519 check as
`verify_player_proof` (RFC 7748 § 6.1, ADR-8665). The hub records `(player_key, server_key, serial, expiry)`.

One owner key may bind many server keys. One server key may carry up to 8 owner proofs (a clan server).
One bad proof fails the whole `RegisterServer`. An empty proof list is the ownerless server (§ 16.3
ownerless). The server does not need the hub's private key to store the bytes; only the hub verifies.

**Server presence state.** `Offline → Handshaking → Online → Draining → Offline`. `RegisterServer` is
accepted only after the server-presence proof matches `NetServer::peer_handshake_hash` for that link. The
session identity is the `ServerKey`, not a `PlayerKey`. A second link under the same server key takes the
session back and the old link is dropped (`hub_server_in_use` on the old link), same reconnect rule as a
player key. `capacity` is `1..=4096`, `load <= capacity`, or `hub_server_invalid` and no session. Heartbeats
use the existing `idle_ms`. Silence moves the server to `Offline`, clears its offer, and closes lobbies
that name it (members get `Left`). `Draining` is the hub's drain: new `RegisterServer` and new lobbies are
refused `hub_draining`; online servers stay until their links end.

**Delegated lobby state.** `WaitingOffer → Open → Brokering → Grace → Closed`, plus `Closed` from `Close`
at any state. `CreateDelegated` from a proven player key, while the server is `Online`, with a live binding
of that key to that server, creates a lobby whose kind is `dedicated`, whose `relay_only` starts as the
server's flag, and whose stored address is empty. Otherwise:

| Condition | Code | When |
|---|---|---|
| Server not online | `hub_server_offline` | create |
| No live binding for this owner and server | `hub_binding_unbound` | create |
| Binding expiry in the past | `hub_binding_expired` | create, and the row flips to expired |
| Binding serial in the revocation set | `hub_binding_revoked` | create |
| Server has owners and this key is not one | `hub_binding_not_owner` | create |
| Caller is not a player session | `hub_not_a_player` | create |
| Owner calls `confirm_session` or `set_data("regia.server")` on it | `hub_address_in_data` | any |
| Owner calls `offer_reach` on it | `hub_not_session` | any |

The server session is the only caller of `offer_reach` and `approve` for that lobby. `LobbyAssigned` tells
it to offer. The owner's `auto_approve` does not approve a delegated ask. The server approves from its own
policy, in order: `load < capacity`, the player key is not in the ban set it published and not in
`bans.ron`, the password gate would accept (a missing password fails closed when the server has a password),
then `JoinTicketCheck` when the ask carries a store ticket. A refusal is `Approve(false)` and
`ReachDenied`, with no plan. The owner keeps kick, mute, close, settings, visibility, invites and an
explicit transfer. Transfer to a player key that is also a live owner of that server succeeds. Transfer to
the server key, or to a key that is not an owner, is `hub_binding_not_owner` and changes nothing.
Ownership is never assigned to the server session.

**Revocation.** `RevokeBinding` from the proven owner inserts `(player_key, server_key, serial)` into the
revocation set in that call, before the call returns. The next `CreateDelegated` by that owner for that
server returns `hub_binding_revoked` and creates nothing. Lobbies that owner already created stay up (the
acceptance row closes new-lobby rights, not the match). The set is capped at 4096. At the cap the call is
`hub_revoke_full` and the new serial is not added. A revocation whose expiry is already in the past may be
dropped to make room; a no-expiry revocation is not dropped. The process writes `revocations.ron` in the
log dir by the existing temp-and-rename rule and loads it before it accepts sessions. A restart keeps the
set.

**Owner leaves.** This amends the NL-04 hand-off for delegated lobbies only. Listen lobbies still hand
ownership to the earliest remaining member and still withdraw a confirmed session (ADR-8633). When the
owning player session of a delegated lobby disconnects or idles out:

- `leave_inner` does not run the hand-off and does not emit `OwnerChanged`.
- The lobby's owner remains that `PlayerKey`. The member id is empty until the same key reconnects
  (NL-05 reconnect restores the session).
- State becomes `Grace`. The server's `grace_ms` is the deadline. `0` means no deadline (indefinite):
  `expire` does not close it. Any other value is the clamp from § 16.2. The default a server omits is
  `600_000` (ten minutes): long enough for the owner's client to restart, short enough that an abandoned
  lobby does not sit in the directory. `86_400_000` is the 24 h wire cap so a `u32` cannot pin a lobby for
  49 days.
- The server's offer, the members, and brokering stay. New asks still run.
- `CloseLobby` from the owner, a server `Offline`, or the grace deadline moves the lobby to `Closed`:
  members get `Left`, the server gets `GraceEnded`, the room is deleted. The game process is the server's.
  The hub does not stop it and does not make the server the owner. The server's own default after
  `GraceEnded` is to keep the match and refuse new hub joins.

**Ownerless public server.** `RegisterServer` with zero proofs is online and ownerless. That server session
may `create` one or more public lobbies (kind `dedicated`, owner displayed as the server's handle, no
player owner). It may `CloseLobby` those lobbies. It may not `CreateDelegated`. A later `RegisterServer`
that adds owner proofs is `hub_binding_invalid` until the ownerless lobbies are closed (a server does not
change from community-public to clan-owned under the same key while lobbies are up). This is the path
request 1.7 asks for. It does not exist on `server_host` today (§ 16.1 a).

**Failure.** A refused create, offer, ask or revoke leaves the previous state in place and pushes no plan
and no candidate bytes. `HubHost` counts the code on `HubReport::refused` as it does for every other
`HubError`.

### 16.4 NL-08 — relay-only reach

`relay_only` is set on the server presence, on the lobby (`SetReachMode`, owner or the bound server), or on
one side's `ReachOffer`. The lobby flag and the server flag apply to every member. A member's own
`relay_only` applies to that member's exchange only, so one member can hide an address while another member
of the same listen lobby still punches.

When the effective flag is set for an exchange, `broker_reach` returns `Relay` if the session's offer has a
relay and a join code, and `NoReach` otherwise. It does not return `Mapped` or `Punch`. `build_plans` (the
function `approve_inner` calls) then:

- puts no candidates and no mapped address on either plan;
- does not copy the member's candidates to the session, or the session's candidates to the member;
- rewrites the member's ticket to `JoinTicket::from_game_socket(server_key)` (unspecified address, port 0,
  the same server key) so the ticket bytes carry no `SocketAddr`;
- leaves the relay address and the join code on the member's plan (the relay is the community relay, not
  the session);
- leaves the session's ticket empty, as today.

`HubReach` runs that plan with `JoinPath::relay_first` (no introduction, allocation at once). The session's
`RelayRegistrar`, when the lobby is relay-only or the member's plan said so, does not put host, reflexive
or mapped candidates in `Register`, does not send a punch introduction, and does not answer connectivity
checks for that member. The member's `JoinPath` does not send local candidates. The relay address may appear.
The session's address and the member's address may not.

A listen lobby with no flag keeps today's order: mapped, then punch with the relay at 1.5 s, then relay when
both NAT classes are known and `predict` is `NeedsRelay`.

The wire test records the `Plan` datagrams and the relay control datagrams (`Register`, `PunchIntro`, check
datagrams) and fails if the other side's address bytes appear. Asserting on the struct fields alone does
not pass it.

### 16.5 NL-09 — address exposure, AskReach limits, bans

**Where an address may exist.** Only in the brokering exchange between one member and one session: that
member's `ReachAsk` / `Plan` and that session's `Plan`, and only when the effective `relay_only` flag is
clear. Directory pages, lobby queries, presence rows, server cards, room snapshots, `SessionConfirmed`,
invites, `HubReport` and `lobby()` data have no `SocketAddr`. `regia.server` is not stored. Listen reach
uses the plan's candidates, not lobby data. The existing outsider byte scan in
`the_session_address_goes_to_members_only_and_moves_presence` stays; the member half of that test becomes
the stronger assertion (members also see no address bytes) and presence still moves when the session is
confirmed. The lane must not delete the outsider assertion to make the new one true.

**AskReach limits.** Checked at the start of `ask_inner`, before any candidate is cloned into an event.
Order: hub-operator ban, lobby ban, server ban, server full (`load >= capacity` → `hub_server_full`),
not a member (`not_in_lobby` / `lobby_not_found` as today), lobby not visible to a non-member
(`lobby_not_found`, no existence oracle), lobby not joinable or at `max_members` for a non-member
(`hub_lobby_closed` / `hub_lobby_full`), in-flight caps, then the 10 s windows. A member who already joined
a listen lobby that later filled or closed may still ask. A dedicated server at `load >= capacity` refuses
everyone, members included. A refusal pushes no `ReachAsk` and no `Plan`.

| Limit | Value | Rationale |
|---|---|---|
| Per player key | 4 asks / 10 s | Two joins and two retries inside the window. Walking all 256 listed lobbies would take 640 s, and each ask still requires a join the existing caps already bound. |
| Per source address | 16 asks / 10 s | Four player keys behind one NAT, each at the key budget. Checked first, so many keys on one address share this budget. |
| In-flight per key | 2 | One open exchange plus one retry. An exchange is a join in `Asked` or `Planned`. |
| In-flight on the hub | 64 | Bounds stored candidate lists. Past it, `hub_ask_inflight`. |
| Existing `AdmitGate` | 30/s per address, 20/s per identity | Unchanged, and still in front of the core for every request. |

Windows are deques of `now_ms` on `HubServer`, so the sans-IO test advances time by passing `now_ms`. No
thread sleep. The address key is the `IpAddr` `AdmitGate` already uses; a missing address skips the
address window and still takes the key window.

**Bans.** A `PlayerKey` banned by the lobby owner (`SetBans`), by the server (the key list on
`RegisterServer`, refreshed by a later register, matched against `bans.ron` on the server before it
publishes) or by the hub operator (the ban file) is `hub_banned` at `AskReach`, before any candidate bytes
are written. The same operator ban and lobby ban are checked again in `approve_inner`, with the dedicated
server-seat ban and `load >= capacity`. A ban or a full server that lands while the ask is open is
`ReachDenied` and `Ok(None)`, and no `Plan` is built. Address bans stay on the dedicated host's `bans.ron`
and are not a hub directory field. The hub file is player keys only, so a ban list cannot publish an address.

### 16.6 NL-07 admin path (out)

Remote admin is not a hub message and not a relay allocation in this row. The dedicated host already
authenticates an operator with `admin.token` (`server_host`, `[admin] token_file`) on the server's own
link. The owner who deployed the server uses that path. The hub does not carry admin commands, does not
learn the admin port, and does not open a second stream. A later row may broker a separate relay allocation
for admin. It must still not put admin bytes on the hub socket.

### 16.7 NL-10 — ECS-free hub, relay and join surface

Move the socket-agnostic pieces out of the `ecs` module so `cargo check -p regia_net --no-default-features`
links none of `regia_ecs`, `regia_app`, `regia_scene`, `regia_asset` or `regia_platform` and still exports:

- `relay_split`, `JoinPath`, `JoinSocket`, `JoinRoute`, `fresh_allocation_key` (today `relay_path.rs`);
- `JoinTicket`, `JoinError`, and `RelayRegistrar<S: DatagramSocket + Send>` (today `join.rs`, where the
  socket field is `LinkSocket`);
- `HubReach<S: DatagramSocket>` and `ReachSocket` (today `reach.rs`).

`RelayRegistrar` is generic over the socket. `LinkSocket` stays the App alias and is one allowed `S`.
`server_host` re-exports the moved types at the old paths so existing call sites keep compiling.
`server_host/mod.rs` keeps `regia_app` and `regia_ecs`; the dedicated runtime stays behind `ecs`. The new
module (`regia_net::join_exec`, name fixed by the lane) imports `socket`, `nat`, `relay`, `crypto`,
`host_check` and `port_map` only. Those are already core.

`HubReach::new(socket, &plan, client_key)` is the member's executor on a caller-owned game socket. The
game polls `update(now_ms)` and `take_socket`. No `World`, no `App`, no plugin. `JoinTicketCheck` stays
where it is (already core). A relay-only plan still uses `relay_first` and sends no candidate bytes (§ 16.4).

### 16.8 NL-11 — one process for the hub and the relay

Extend `regia-lobby-hub`. Do not add a second binary. `hub.toml` `format_version` becomes 2. A v1 file
migrates by appending the defaults below (the build lane adds the migrator, `regia upgrade-project`
support, and the format doc; this spec lane does not edit the frozen file). Unknown keys remain a named
field error and exit 2.

```toml
format_version = 2
# v1 keys unchanged: bind, port, key_path, caps, limits.per_address_per_sec,
# limits.per_identity_per_sec, status.

[limits]
ask_per_identity_per_10s = 4
ask_per_address_per_10s = 16
ask_inflight_per_identity = 2
ask_inflight_hub = 64

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

`relay.enabled = false` is the v1 behavior after migration (the migrator writes `false` so an existing hub
does not open a new port until the operator turns it on). A fresh package writes `true`. The relay port is
not the hub port. `RelayServer` is polled from the same `TestClock` / process tick as `HubHost`. SIGTERM or
SIGINT calls the existing hub drain and stops new relay allocations; refresh of allocations already issued
continues until they are gone or 30 s of that clock have passed, then the process exits 0. `GET /health`
and `lobby.hub_status` gain `relay_allocations`, `servers_online` and `draining`. HTTP 503 while draining,
as today.

**Key rotation.** Clients keep pinning one hub `ServerKey` (`HubLobbyBackend::connect` still refuses an
unpinned key). The process has one Noise static key (the `snow` builder takes one local static). Rotation
is two processes, not two keys in one handshake: bring up the new key on a new port, ship a game config
whose pin list tries the new hex key and then the old one, wait the adoption window, stop the old process,
drop the old hex key from the next game config. The private key stays in `key_path` outside any project
(NL-06). The hex public key is what a game ships; it is not a secret. Overlap longer than 7 days is an
operator choice, not a wire field. A client that pins a key the process does not hold ends the link with
`hub_key_mismatch` and allocates no session.

### 16.9 Agent surface and codes

Build lanes add the methods. This spec does not register them.

- `lobby.hub_servers`: server cards as in § 16.2 (no address). Errors `hub_bad_params`, `hub_unknown`.
- `lobby.hub_bindings`: the caller's own bindings (server key, serial, expiry, state). Not the directory.
- `lobby.hub_status` gains `servers_online`, `relay_allocations`, `ask_refusals`.
- `lobby.hub_join_status` gains `relay_only` and `grace` per lobby.

Book page for the build lanes: `book/agents/net-delegated-sessions.md`, with a `book/SUMMARY.md` link and
`book/COVERAGE.md` rows appended at the end of the relevant tables. Fuzz target `net.lobby_hub_msg` grows
the new messages; `net.hub_toml` grows format v2. Neither target is weakened.

New `HubError` codes, each `lowercase` digits and `_` only: `hub_binding_unbound`, `hub_binding_expired`,
`hub_binding_revoked`, `hub_binding_not_owner`, `hub_binding_invalid`, `hub_server_offline`,
`hub_server_in_use`, `hub_server_invalid`, `hub_server_full`, `hub_not_session`, `hub_not_a_player`,
`hub_ask_rate_limited`, `hub_ask_inflight`, `hub_banned`, `hub_address_in_data`, `hub_lobby_closed`,
`hub_lobby_full`, `hub_revoke_full`.

### 16.10 Acceptance

Tests use `TestClock` or the core's `now_ms`, and `SimNat` where a path is claimed. A passing loopback with
no NAT does not satisfy a row that names a path. The seven rows from the request:

1. Delegated lobby: owner A binds server S; A creates a lobby on S; member B joins through the hub; B's sealed handshake completes with S's pinned ServerKey by each path (mapped, punch, relay; SimNat matrix); B never learns A's address.
2. Binding failures: unbound server, expired binding, server bound to another owner are refused at lobby creation; a revoked binding closes that owner's new-lobby rights at once.
3. Relay-only: member receives no host candidates and host receives no member candidates; checked on the wire bytes, not just the API.
4. Listings: no listing, query, presence event or lobby-data read contains an address.
5. Rate limits and bans: the AskReach rate limit and in-flight cap hold under a flood from one key and from many keys on one address; a banned key gets no candidates.
6. Owner leaves mid-match: the session continues for the grace period.
7. ECS-free build: compiles with `default-features = false` and no `ecs`, and runs a JoinPlan on a caller-owned socket.

Added:

8. An ownerless server registers a public lobby. A member joins it. The directory card has kind `dedicated` and no address bytes.
9. A restarted hub loads `revocations.ron` and refuses that owner's next `CreateDelegated`.
10. A version-1 client and a version-2 hub refuse each other with `protocol_mismatch` before a session exists.
11. `AskReach` on a full dedicated server, a closed lobby (non-member) and an invisible lobby (non-member) returns no candidate bytes. A member already inside a closed listen lobby still receives a plan.
12. An `RGN1` datagram on the hub socket is not forwarded. An `RGNR` datagram on the hub socket is not forwarded.
13. A binding proof whose static key is a low-order point is `hub_binding_invalid`.
14. Of two owner proofs on one server, owner B creates a lobby and owner C, who has no proof, is `hub_binding_not_owner`.
15. `grace_ms = 0` keeps the delegated lobby after the owner disconnects, with no `OwnerChanged`, until `CloseLobby` or the server goes offline.
16. A relay-only member plan's ticket bytes contain no address. The session's plan bytes contain no address of the member.
17. `cargo tree -p regia_net --no-default-features -e normal` does not list `regia_ecs`, `regia_app`, `regia_scene`, `regia_asset` or `regia_platform`.

Lane ownership of these rows is in the lane report for ADR-10375.

## 17. Sources

Public sources for § 16. No other engine's source was read.

- T. Perrin, "The Noise Protocol Framework", revision 34, § 7.4 pattern `X` and § 11.2 channel binding.
  https://noiseprotocol.org/noise.html
- RFC 7748 (X25519, low-order check § 6.1). https://www.rfc-editor.org/rfc/rfc7748
- RFC 8445 (ICE: candidates, connectivity checks, nomination; a response comes from the address the check
  used). https://www.rfc-editor.org/rfc/rfc8445
- RFC 8489 (STUN). https://www.rfc-editor.org/rfc/rfc8489
- RFC 8656 (TURN-style relay allocations and permissions). https://www.rfc-editor.org/rfc/rfc8656
- RFC 4787 § 4.1 and § 5 (mapping and filtering), already the model behind `relay::simulate_hole_punch`.
  https://www.rfc-editor.org/rfc/rfc4787

`PlayerKey` is X25519. RFC 8032 (Ed25519) is not the binding construction. The proof is a Noise `X` message
to the hub static key, which is the primitive `crypto` already uses.
