# Net lobbies (agents)

Lobbies, matchmaking and the Steam sockets transport of `regia_net` and `regia_steam` (spec 89, ADR-1877). Guide
topic `net-lobbies`. A lobby is where players gather before a game session; the session itself is
[net session](net-session.md).

A lobby is a small platform-held room: key/value data, per-member data, a member limit, a visibility (`public`,
`friends_only`, `private`, `invisible`) and a joinable flag. The game drives one `Lobbies` resource
(`regia_net::Lobbies`, pumped by `LobbyPlugin` once per frame in `PreUpdate`). Two backends:

| Backend | `lobby.status` `backend` | Where |
|---|---|---|
| Local (in-process directory) | `local` | `regia_net::LocalLobbyHub`: tests, play-in-editor with several clients, builds without a platform. Deterministic. |
| Steam (ISteamMatchmaking) | `steam` | `regia_steam::SteamLobbyPlugin` attaches it once Steam is ready. |
| None | `none` | No platform: every call is `lobby_unavailable`. |

Platform waits (`lobby.create`, `lobby.join`, `lobby.list`) answer with a `request` id at once; the result arrives on
a later frame as an event (`created`, `entered`, `list`) in `lobby.status` `recent_events` (and `last_list` for
lists). Watch them with `wait_until` on `lobby.status`.

## Compatible lobbies only

Lobbies the engine creates carry engine keys (`regia.v`, `regia.game`, `regia.protocol`, `regia.content`,
`regia.state`, and after launch `regia.server`). `lobby.list` and `matchmaking.start` always add filters for the same
game, protocol hash and (unless `lobby.match_content` is false) content hash, so a different build never shows up.
The hashes come from the app's `NetSession` manifest, the values the session's content handshake compares:
`LobbyPlugin` copies them at startup and keeps them in step every frame (mods mounted at session start change the
content hash), unless the game set its own with `Lobbies::set_identity(game, &manifest)`. The game id
(`regia.game`) is empty unless the game names it (`Lobbies::set_identity`, or `Lobbies::set_game` to keep the session
hashes). An app with no `NetSession` and no identity of its own is refused: `lobby.create`, `lobby.list` and
`matchmaking.start` answer `lobby_no_identity`, unless the game opted in with `Lobbies::allow_unidentified(true)`
(then every build that did the same meets every other one, the old behavior). `lobby.status` `identity` shows
`{ source, game, protocol, content, protocol_hash, content_hash, from_manifest, allow_unidentified }`: `source` is
`session`, `explicit` or `unset` (ADR-7881); `protocol_hash` and `content_hash` repeat `protocol` and `content` under
ADR-7213's names. When `create_if_none`, `matchmaking.start` checks its create fields when it starts, so a
bad `max_members` or too many `data` keys answers `members_invalid` or `too_many_keys` at once. A join-only call
(`create_if_none: false`) never creates, so it ignores `max_members` and the key count and just searches.
Keys starting `regia.` cannot be set by the game (`reserved_key`); use `lobby.ready` for the ready flag
(member key `regia.ready`).

The protocol and content hashes come from the session's manifest (`NetSession::manifest()`) until the game calls
`Lobbies::set_identity` (spec 89 § 4, ADR-7213); the game id stays empty unless the game sets it. `lobby.status`
`identity` shows what is in force: `{ source, game, protocol_hash, content_hash, from_manifest, ... }` (hashes as 16 hex digits;
`from_manifest: false` once the game set its own).

## Queries

`strings: [{ key, op?, value }]` compare bytes; `numbers: [{ key, op?, value }]` compare the lobby's value parsed as
an i32 (unparsable never matches); `op` is `eq` (default), `ne`, `lt`, `le`, `gt` or `ge`; a missing key fails every
filter except `ne`. `near: [{ key, value }]` sorts closest first (earlier keys dominate). `open_slots` asks for free
slots, `distance` is `close`, `default`, `far` or `worldwide` (Steam regions; the local backend treats all lobbies as
close), `max_results` is 1..=50. Only public, joinable, non-full lobbies are listed. The local backend orders ties by
member count (most first) then lobby id; Steam uses its own order.

## Matchmaking

`matchmaking.start` lists in rounds that widen the distance (`lobby.rounds` in `config/net.ron`, default close,
default, far, worldwide), joins the first candidate (a full one moves to the next), and creates a lobby when every
round is empty (`create_if_none`, default true). `matchmaking.status` `state`:

| State | Meaning |
|---|---|
| `idle` | Not matchmaking. |
| `listing` | Waiting for round `round`'s list. |
| `joining` | Waiting to enter `lobby`; `candidates` are the lobbies left to try. |
| `creating` | Waiting for the fallback lobby. |
| `in_lobby` | In `lobby`; `ready_to_launch` is true for the owner when at least `min_players` are in and all are ready. |
| `launched` | The owner launched; `server` is where the session is. |
| `failed` | `error`: `no_match`, `create_failed`, `request_timeout` (no answer to the fallback create for `lobby.step_timeout` frames; an unanswered list counts as an empty round and an unanswered join moves to the next lobby), `left` (kicked or the lobby vanished), `unavailable`. |

Waits are counted in frames, never wall time, so a fixed-step run replays the same choices.

## Launch and connect

The owner calls `lobby.launch`. Without `address` it hosts a listen session on the lobby transport and publishes it:
over Steam sockets the server is `steam:<owner id>`, over the loopback transport `udp:127.0.0.1:<port>`. With
`address` it publishes a session hosted elsewhere (`udp:<ip:port>`). The lobby state (`regia.state` in the lobby data)
becomes `launching` and the lobby closes to new joins (unless `lobby.joinable_in_game`). The publish is
all-or-nothing: when a write after the server key is refused (`lobby_platform`), the launch restores `regia.state`
to `open` and removes the server key before failing, so the lobby never advertises a session that is not hosting
(a hosting `lobby.launch` tears the just-hosted session down on the failure). Every member with
`lobby.auto_connect` (default) connects its `NetSession` on the next frame; `lobby.status` `connected_to` shows where,
`connect_error` why not. Then follow the session with `session.state`.

The lobby state becomes `in_game` when the owner's session shows play: a peer in game on a hosted session, the owner
alone in the lobby, or the owner's own client in game on a session hosted elsewhere. An owner who publishes a session
it never joins stays at `launching`, so wait on `session.state` there, not on the lobby state.

## Steam sockets

`regia_steam::SteamSocket` is a `regia_net` datagram socket over Steam peer-to-peer messages (ISteamNetworkingMessages,
unreliable, no Nagle): Steam's relay network does NAT traversal and hides addresses, the spec 77 session does
reliability and ordering. Steam ids appear in session addresses as `[fd52:6567:6961:0:…]:<channel>` (a synthetic
unique-local IPv6 address; `regia_steam::steam_id_of` reads the id back). Datagrams are at most 2048 bytes. By
default any peer may open a session (the session handshake authenticates); `SteamLobbyPlugin { members_only: true }`
accepts only members of the current lobby.

A test or a headless agent run without the SDK puts its `MockSteam` users on one `MockSteamNet`. That network can
impair each user's link with the soak harness's profiles (`MockLink::profile`: `clean`, `good-wifi`, `bad-wifi`,
`mobile-4g`, `transatlantic`, `congested`, `flappy`; ADR-8351), in virtual time (`advance_ms`) with a seed
(`set_seed`). Sessions on it behave as on the impaired network: `session.state` reports `interrupted` during a
`flappy` outage and goes back to normal after it, and a resumed lobby client keeps its `peer_id`.
`MockLink::profile_for_client` adds the harness's per-client extras (`flappy`'s long outage, the removal timeout plus 2 s, on every fourth client,
`mobile-4g`'s NAT rebinding as a reroute gap), and `mobile-4g` fragments messages over its 1,232-byte MTU (ADR-10069).
A rolled-back launch (a refused publish step) clears the server through `LobbyBackend::clear_server`, which on Steam also
overwrites `SetLobbyGameServer` with Steam's no-server values. The mock adds no
agent method and no error code; `net.conditioner_set` still conditions UDP and loopback endpoints.

## Methods

| Method | Params | Result |
|---|---|---|
| `lobby.status` | — | `{ identity, backend, transport, local_member, lobby?, owner, connected_to?, connect_error?, matchmaking, last_list, recent_events }` |
| `lobby.list` | `strings?, numbers?, near?, open_slots?, distance?, max_results?` | `{ request }` |
| `lobby.create` | `visibility?, max_members?, data?` | `{ request }` |
| `lobby.join` | `lobby` (id string or integer) | `{ request }` |
| `lobby.leave` | — | `{ left }` |
| `lobby.set_data` | `key, value?` (null deletes), `member?` | `{ ok }` |
| `lobby.ready` | `ready` | `{ ready }` |
| `lobby.chat` | `text` (1..=4096 bytes) | `{ ok }` |
| `lobby.launch` | `address?, level?` | `{ server }` |
| `matchmaking.start` | `strings?, numbers?, near?, min_players?, create_if_none?, visibility?, max_members?, data?` | `{ state }` |
| `matchmaking.cancel` | — | `{ state }` |
| `matchmaking.status` | — | `{ state, round, candidates, lobby?, ready_to_launch, server?, error? }` |

Lobby and member ids are decimal strings (Steam ids exceed 2^53). A lobby in results is `{ id, owner, visibility,
max_members, member_count, joinable, members: [{ id, name, data }], data, server }`; listings carry `member_count`
with an empty `members`. Event `kind`s: `created`, `entered`, `list`, `member_joined`, `member_left`, `data_changed`,
`owner_changed`, `chat`, `server_set`, `invite_accepted`, `left` (`reason` `left`, `kicked` or `lost`).

## Errors

All are `F0002` with `data.reason`:

| Reason | Meaning |
|---|---|
| `lobby_not_running` | The app has no `Lobbies` resource (add `LobbyPlugin` or `SteamLobbyPlugin`) |
| `lobby_unavailable` | No lobby platform (Steam not running, feature off, Android) |
| `not_in_lobby` | The call needs a lobby this process is in |
| `already_in_lobby` | Already in a lobby or matchmaking; leave first |
| `not_owner` | Only the owner sets lobby data, closes the lobby or launches |
| `lobby_full` | Full or not joinable (in an `entered` event) |
| `lobby_not_found` | No such lobby (in an `entered` event) |
| `key_invalid` | Key empty, over 255 bytes or with a NUL |
| `value_too_long` | Value over 8192 bytes or with a NUL |
| `reserved_key` | `regia.` keys belong to the engine |
| `too_many_keys` | More than 64 keys on a lobby or member |
| `chat_invalid` | Chat text empty, over 4096 bytes or with a NUL |
| `members_invalid` | `max_members` outside 1..=250 |
| `query_invalid` | A filter out of range |
| `lobby_platform` | The platform refused (a Steam result) |
| `lobby_no_identity` | Create, list or matchmaking with no lobby identity (no `NetSession`, no `set_identity`/`set_game`, no `allow_unidentified`) |
| `matchmaking_busy` | `matchmaking.start` while searching |
| `no_transport` | `lobby.launch` without `address` and no lobby transport |
| `session_error` | Hosting the session failed (`session.state`) |
| `net_not_running` | `lobby.launch` without `address` in an app with no `NetSession` |

## Settings

The `lobby` section of `config/net.ron` ([format](../../docs/formats/net-ron.md)):

| Field | Default | Meaning |
|---|---|---|
| `lobby.max_members` | 8 | Member limit of lobbies the engine creates |
| `lobby.visibility` | `Public` | Visibility of lobbies the engine creates |
| `lobby.min_players` | 2 | Members needed before `ready_to_launch` |
| `lobby.match_content` | true | Also require the same content hash |
| `lobby.auto_connect` | true | Members connect the session when the owner launches |
| `lobby.joinable_in_game` | false | Keep the lobby joinable after launch |
| `lobby.rounds` | `[Close, Default, Far, Worldwide]` | Matchmaking rounds |
| `lobby.results_per_round` | 50 | Results per list round (1..=50) |
| `lobby.step_timeout` | 600 | Frames to wait for a platform answer |
| `lobby.create_if_none` | true | Create a lobby when matchmaking finds none |
| `lobby.steam_channel` | 0 | Steam sockets channel of the session. A value above 65535 is used as channel 0 |

## Example: two processes matchmake and play

```text
A: matchmaking.start { strings: [{ key: "mode", value: "duel" }], data: { mode: "duel" } }
A: wait_until matchmaking.status state == "in_lobby"          # nothing found: A created the lobby
B: matchmaking.start { strings: [{ key: "mode", value: "duel" }] }
B: wait_until matchmaking.status state == "in_lobby"
A: lobby.ready { ready: true }   B: lobby.ready { ready: true }
A: wait_until matchmaking.status ready_to_launch == true
A: lobby.launch { level: "levels/arena.level.ron" }
B: wait_until session.state state == "in_game"
```

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `lobby.chat`

Sends a chat message to every member of the current lobby.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `text` | string | yes | 1..=4096 bytes. |

Result: `{ ok }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`. Backend refusals use `F0002`; the backend error vocabulary is `lobby_unavailable`, `not_in_lobby`, `already_in_lobby`, `not_owner`, `lobby_full`, `lobby_not_found`, `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`, `chat_invalid`, `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`. A create, join or list can also report an asynchronous refusal in `lobby.status.recent_events[].error`.

```json
lobby.chat {"text": "gl hf"}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:lobby.chat -->

### `lobby.create`

Creates a lobby we own with the engine keys; the result arrives as a created event.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `visibility` | string | no | public, friends_only, private or invisible (default from config/net.ron). |
| `max_members` | integer | no | 1..=250. |
| `data` | object | no | Game keys and string values (keys starting regia. are refused). |

Visibility and max_members default to the lobby settings (initially public and 8); data defaults to an empty object. Keys are 1..=255 UTF-8 bytes, at most 64 keys, and values at most 8192 bytes; neither may contain NUL.

Result: `{ request }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`, `already_in_lobby`. Backend refusals use `F0002`; the backend error vocabulary is `lobby_unavailable`, `not_in_lobby`, `already_in_lobby`, `not_owner`, `lobby_full`, `lobby_not_found`, `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`, `chat_invalid`, `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`. A create, join or list can also report an asynchronous refusal in `lobby.status.recent_events[].error`.

```json
lobby.create {"visibility": "public", "max_members": 4, "data": {"mode": "duel"}}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:lobby.create -->

### `lobby.join`

Joins a lobby by id; the result arrives as an entered event.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `lobby` | string | yes | Lobby id (decimal string or integer). |

Result: `{ request }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`, `already_in_lobby`. Backend refusals use `F0002`; the backend error vocabulary is `lobby_unavailable`, `not_in_lobby`, `already_in_lobby`, `not_owner`, `lobby_full`, `lobby_not_found`, `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`, `chat_invalid`, `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`. A create, join or list can also report an asynchronous refusal in `lobby.status.recent_events[].error`.

```json
lobby.join {"lobby": "1"}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:lobby.join -->

### `lobby.launch`

Owner: publishes where the session is. Without address, hosts a listen session on the lobby transport (Steam sockets or loopback) and publishes it; members auto-connect.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `address` | string | no | ip:port of a session hosted elsewhere. |
| `level` | string | no | Starting level when hosting. |

Result: `{ server }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`, `net_not_running`, `no_transport`, `session_error`, `not_in_lobby`, `not_owner`. Backend refusals use `F0002`; the backend error vocabulary is `lobby_unavailable`, `not_in_lobby`, `already_in_lobby`, `not_owner`, `lobby_full`, `lobby_not_found`, `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`, `chat_invalid`, `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`. A create, join or list can also report an asynchronous refusal in `lobby.status.recent_events[].error`.

```json
lobby.launch {"level": "levels/arena.level.ron"}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:lobby.launch -->

### `lobby.leave`

Leaves the current lobby and stops matchmaking.

*Play, mutating.*

No parameters.

Result: `{ left }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`.

```json
lobby.leave {}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:lobby.leave -->

### `lobby.list`

Lists compatible lobbies (same game, protocol and content hash added by the engine); the result lands in lobby.status last_list on a later frame.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `strings` | array | no | [{ key, op?, value }] string comparisons (op eq, ne, lt, le, gt, ge; default eq). |
| `numbers` | array | no | [{ key, op?, value }] i32 comparisons. |
| `near` | array | no | [{ key, value }] sort closest first. |
| `open_slots` | integer | no | At least this many free slots. |
| `distance` | string | no | close, default, far or worldwide. |
| `max_results` | integer | no | 1..=50 (default 50). |

String, number and near filters default to empty arrays; comparisons default to eq. At most 64 filters, max_results defaults to 50, distance to default, and omitted open_slots imposes no extra minimum. Open_slots is clamped to 250.

Result: `{ request }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`, `query_invalid`. Backend refusals use `F0002`; the backend error vocabulary is `lobby_unavailable`, `not_in_lobby`, `already_in_lobby`, `not_owner`, `lobby_full`, `lobby_not_found`, `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`, `chat_invalid`, `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`. A create, join or list can also report an asynchronous refusal in `lobby.status.recent_events[].error`.

```json
lobby.list {"strings": [{"key": "mode", "op": "eq", "value": "duel"}], "open_slots": 1}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:lobby.list -->

### `lobby.ready`

Marks the local member ready or not (member key regia.ready).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `ready` | boolean | yes | Ready or not. |

Result: `{ ready }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`. Backend refusals use `F0002`; the backend error vocabulary is `lobby_unavailable`, `not_in_lobby`, `already_in_lobby`, `not_owner`, `lobby_full`, `lobby_not_found`, `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`, `chat_invalid`, `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`. A create, join or list can also report an asynchronous refusal in `lobby.status.recent_events[].error`.

```json
lobby.ready {"ready": true}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:lobby.ready -->

### `lobby.set_data`

Sets or deletes a game key of the current lobby (owner only) or of the local member's data.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `key` | string | yes | The key (1..=255 bytes, not regia.). |
| `value` | string | no | The value (at most 8192 bytes); null or absent deletes. |
| `member` | boolean | no | true: the local member's data instead of the lobby's. |

Result: `{ ok }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`. Backend refusals use `F0002`; the backend error vocabulary is `lobby_unavailable`, `not_in_lobby`, `already_in_lobby`, `not_owner`, `lobby_full`, `lobby_not_found`, `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`, `chat_invalid`, `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`. A create, join or list can also report an asynchronous refusal in `lobby.status.recent_events[].error`.

```json
lobby.set_data {"key": "map", "value": "harbor"}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:lobby.set_data -->

### `lobby.status`

Lobby state (spec 89): backend, transport, local member, the current lobby with members and data, matchmaking, the last list and recent lobby events.

*Read, does not mutate.*

No parameters.

Result: `{ backend, transport, local_member, lobby?, owner, connected_to?, connect_error?, matchmaking, last_list, recent_events: [{ kind, lobby?, ... }] }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`.

```json
lobby.status {}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:lobby.status -->

### `matchmaking.cancel`

Stops matchmaking and leaves the lobby it joined or created.

*Play, mutating.*

No parameters.

Result: `{ state }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`.

```json
matchmaking.cancel {}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:matchmaking.cancel -->

### `matchmaking.start`

Starts matchmaking: list compatible lobbies in widening rounds, join the best, create one when none fits (spec 89 section 5).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `strings` | array | no | Game string filters, as lobby.list. |
| `numbers` | array | no | Game number filters. |
| `near` | array | no | Near-value sort keys. |
| `min_players` | integer | no | Members needed before ready_to_launch. |
| `create_if_none` | boolean | no | Create a lobby when nothing fits (default true). |
| `visibility` | string | no | Visibility of a created lobby. |
| `max_members` | integer | no | Member limit of a created lobby. |
| `data` | object | no | Game keys of a created lobby. |

Filter defaults are those of lobby.list. Visibility, max_members, min_players and create_if_none come from lobby settings (initially public, 8, 2 and true). A supplied min_players is clamped to 1..=250; data defaults to empty.

Result: `{ state }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`, `already_in_lobby`, `query_invalid`, `matchmaking_busy`. Backend refusals use `F0002`; the backend error vocabulary is `lobby_unavailable`, `not_in_lobby`, `already_in_lobby`, `not_owner`, `lobby_full`, `lobby_not_found`, `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`, `chat_invalid`, `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`. A create, join or list can also report an asynchronous refusal in `lobby.status.recent_events[].error`.

```json
matchmaking.start {"strings": [{"key": "mode", "value": "duel"}], "min_players": 2}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:matchmaking.start -->

### `matchmaking.status`

The matchmaker: state (idle, listing, joining, creating, in_lobby, launched, failed), round, candidates, lobby, ready_to_launch, server, error.

*Read, does not mutate.*

No parameters.

Result: `{ state, round, candidates, lobby?, ready_to_launch, server?, error? }`.

Errors: `F0002`. `data.reason`: `lobby_not_running`.

```json
matchmaking.status {}
```

```json,ignore
→ {
  "message": "the app has no regia_net Lobbies resource",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    "reason": "lobby_not_running"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:matchmaking.status -->

## Workflow: create a lobby, ready up, and launch

1. `lobby.create` with `visibility`, `max_members` and a small `data` object (the mode, the map).
2. Poll `lobby.status` until creation completes and inspect recent_events for failures. Keep the lobby id. The joining process calls lobby.list, waits for last_list, then lobby.join and waits until its status shows that lobby.
3. `lobby.set_data` for a key the host owns, `lobby.ready` true for each member, `lobby.chat` for a line the members should see.
4. `lobby.launch` when every member is ready. `lobby.status` shows the launch.
5. `lobby.leave` when the session should end. `matchmaking.start` is the other path: start, poll `matchmaking.status`, `matchmaking.cancel` if the player backs out.

<!-- /depth-2:method-reference -->

## Mode and playlist keys

A host that runs a match session publishes two optional engine keys (ADR-8897), so a server browser can filter by
mode and a matchmaker can look for a playlist:

| Key | Value |
|---|---|
| `regia.mode` | The running mode: the current playlist row's `mode` (a rule-set id or a mode string), else the board's mode lobby id (`ctf`, `koth`, `tdm`, ...). |
| `regia.playlist` | The playlist's id (`Playlist::id`; a dedicated host uses the `.playlist.ron` file name). Absent when the playlist has none. |

The host calls `Lobbies::publish_session(&session)` after each launch (or `Lobbies::set_rotation_keys(mode,
playlist)`). A lobby created afterwards carries the keys, and the owner of the current lobby writes them at once; an
empty value deletes its key. The game cannot set `regia.` keys itself (`reserved_key`). A bad value answers
`value_too_long`. Filter with `LobbyQuery::with_mode("ctf")` / `LobbyQuery::with_playlist("public-ctf")`, or over
the protocol with a `strings` filter:

```json
{ "method": "lobby.list", "params": { "strings": [{ "key": "regia.mode", "op": "eq", "value": "ctf" }] } }
{ "method": "matchmaking.start", "params": { "strings": [{ "key": "regia.playlist", "op": "eq", "value": "public-ctf" }], "create_if_none": false } }
```

The keys make the lobby-keys layout v2 (`regia.v` is `2`, `docs/formats/lobby-keys.md`). They are optional, so a v1
build ignores them and a v1 lobby simply has neither.
