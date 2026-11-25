# Lobby hub status (agents)

A self-hosted lobby hub (`regia_net::lobby::hub`, ADR-8559) gives games lobbies, a directory with
presence and invites without a platform account. The hub process runs a `HubHost` (a `NetServer`
that always seals under the hub's Noise identity plus the `HubServer` state core); clients use
`HubLobbyBackend`, a `LobbyBackend` pinned to the hub's `ServerKey`. See
[Networking: self-hosting a lobby hub](../user-guide/networking.md#self-hosting-a-lobby-hub).

`lobby.hub_status` reports the hub reports a process published. A hub process publishes with
`HubHost::publish(label, &mut NetAgentState)` (or `NetAgentState::publish_hub(label,
HubServer::report())`), for example once a second.

| Method | Params | Result |
|---|---|---|
| `lobby.hub_status` | `hub` (optional label) | `{ hubs: [ report ] }` |

Each report:

| Field | Meaning |
|---|---|
| `label` | The label it was published under. |
| `sessions` | Hub sessions (a session exists only after the client's hello, over a finished handshake). |
| `lobbies`, `listed` | Lobbies, and of those the public (listed) ones. |
| `invites` | Open invites (each expires after `invite_ttl_ms`, 2 h by default). |
| `generations` | `{ directory, presence }`: the generations the directory and the presence rows are at. They move only when a row changes. |
| `state_bytes` | The state core's estimated memory, bounded by the caps. |
| `bytes_sent` | `{ directory, chat, other }`: bytes of events queued to clients, by kind. Idle sessions cost 0 directory bytes. |
| `expired` | Sessions expired (silent for `idle_ms`, 30 s by default, or slow readers: a session whose outbox overflowed `outbox_cap` and could not drain; one that drains gets a resync instead, ADR-8633). |
| `refused` | Refused operations by code (below), plus the counters `hub_no_hello` (a connection that never said hello), `hub_send_failed` (an event the link refused outright), `hub_list_rewind` (a list whose cursor moved backwards inside `list_rewind_ms`, 10 s, ignored) and `hub_proof_dropped` (a link dropped at its fourth refused player proof, `PROOF_FAILURE_LIMIT`; a Hello whose proof verified but whose session was refused, such as a bad or taken handle, counts too, ADR-8883; its later proofs are not checked, ADR-8734). |
| `players` | At most 64 presence rows: `member` (decimal string), `handle`, `presence` (`menu`, `lobby`, `in_game`), `lobby` (only when that lobby is public, else `null`). |
| `games` | At most 64 listed lobby cards: `id`, `owner`, `owner_handle`, `visibility`, `max_members`, `member_count`, `joinable`, `data`. |

No report carries a session address: directory cards drop `regia.server`, presence rows and invite
cards have no address field, and the address goes only to a lobby's members once the owner
confirms it.

```json
{ "method": "lobby.hub_status", "params": {} }
{ "method": "lobby.hub_status", "params": { "hub": "hub" } }
```

## Errors

| Code | When |
|---|---|
| `hub_bad_params` | Params are not an object, carry a key other than `hub`, or `hub` is not a non-empty string. |
| `hub_unknown` | No report has that label; `lobby.hub_status` with `{}` lists them. |

## Hub refusal codes

These appear in `refused`, in `HubLobbyBackend::last_error`, in `HubLinkState::Ended`, and (as
`lobby_platform` with the hub code as its text) in `LobbyEvent` results:

| Code | Meaning |
|---|---|
| `hub_key_unpinned` | The client was given no hub key; refused before any datagram. |
| `hub_key_mismatch` | The hub answered under another key than the pinned one (Noise IK); no session is allocated. |
| `hub_handle_invalid`, `hub_handle_taken` | The handle is empty, too long (`handle_max_bytes`, 24) or has a control character; another session holds it (case-insensitive). |
| `hub_session_cap`, `hub_lobby_cap` | The hub holds `session_cap` (1024) sessions or `lobby_cap` (256) lobbies. |
| `hub_no_session` | A request before the hello, or after the session expired. |
| `hub_presence_invalid` | `lobby` without a lobby, `in_game` without a confirmed session, `menu` while in a lobby. |
| `hub_too_many_friends` | More than `friend_cap` (64) friends. |
| `hub_invite_only`, `hub_friends_only` | Counted in `lobby.hub_status` `refused` only, never sent to the joiner: a private lobby without a matching, unexpired invite token; a friends-only lobby whose owner does not list the joiner's player key and no invite bound to that key, or a joiner with no player key (its hello carried no proof: refused, fail closed; ADR-8633). Friends are player keys, never handles. The joiner is answered `lobby_not_found`, as for a missing id, so nobody can probe which private lobbies exist (KI-NT-046, ADR-10069). |
| `hub_no_such_player` | An invite to a handle no session holds. |
| `hub_chat_rate_limited`, `hub_chat_invalid`, `hub_muted`, `hub_cannot_mute_owner` | Over `chat_per_sec` (4) per scope; empty or over `chat_max_chars` (160); muted by the owner; the owner cannot be muted. |
| `hub_data_too_large` | Lobby data over `lobby_data_bytes` (8 KiB) or member data over `member_data_bytes` (256 B); a session address that would put the lobby data over its budget or key count. |
| `hub_data_rate_limited` | More than `data_per_sec` (20) lobby-data, member-data, session-address or joinable writes by one session in a second, or `lobby_data_per_sec` (160) in one lobby. Global chat over `global_chat_per_sec` (16) hub-wide is `hub_chat_rate_limited`. |
| `hub_disconnected` | The link to the hub is down. |
| `hub_address_rate_limited` | Too many requests from one address in one second (the process gate, in front of the core). |
| `hub_identity_rate_limited` | Too many requests from one player key in one second. |
| `hub_draining` | The hub is shutting down and takes no new session. |

Lobby codes (`lobby_full`, `not_owner`, `not_in_lobby`, ...) are the ones of
[Lobbies (agents)](net-lobbies.md).

## Fuzzing

`fuzz.run` target `net.lobby_hub_msg` decodes hub requests and events with the hub's bounded
decoder and checks the round trip ([Fuzzing](fuzzing.md)).

## Send rate and overflow (ADR-8633)

`HubHost` sends each peer at most `reliable_msgs_per_s` (1024) reliable messages a second. What the link refuses for that
rate or for back-pressure stays at the front of the session's outbox for the next update; nothing is skipped. An outbox past
`outbox_cap` (1024) drops events and is repaired at the next drain by a resync: the session's room, its invites, a lobby
owner's open asks, and a directory snapshot; since ADR-10069 (KI-NT-045) also the kept chat lines (lobby and global, at
most `chat_history`) from the first one the cap dropped on, in order and once, and a join plan the cap dropped (to the
member, and the owner's copy to the owner). Only a session that cannot drain is dropped (`expired`). When a lobby's owner
leaves, the session address it confirmed is withdrawn for everyone.

Codec generation 2 (ADR-10378) adds `hub_needs_wire_v2` and `hub_not_ready` to this `refused` map.
See [Delegated hub sessions](net-delegated-sessions.md). The handshake protocol stays 1.
