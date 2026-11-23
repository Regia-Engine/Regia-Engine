# Editor collaboration presence

Collaboration is off by default. Open **Collaboration** through the command palette
(`editor.command { "run": "window.collaboration" }`). Tab reaches its named fields and
buttons; Enter/Space activates buttons. UI scale and theme tokens apply.
Closing the window keeps the session active; use Leave to end it. This is a trusted
LAN feature: names are self-reported and traffic is unencrypted. Open the same saved level
in each editor. Presence never transfers assets or edits; graph documents remain single-user.

## Host, join, accept, leave

- `editor.collab_host { "action": "start", "address": "<lan-host>", "name": "Avery" }`
  binds a nonblocking UDP presence socket. `action` defaults to `start`; `address` and `name`
  are required for start. An explicit loopback address and port `0` are useful for local tests;
  the response's `address` reports the allocated port. No DNS, discovery or relay is used.
- `editor.collab_join { "address": "<lan-host>", "name": "Morgan" }` requests entry.
  The result is `joining`; it is not acceptance. The host sees a pending request on its next
  presence tick. The joiner sees no roster, cameras or selections yet.
- `editor.collab_participants {}` reads `state`, `address`, `self_id`, `participants`, `pending`,
  `error` and traffic counters. Each participant has `id`, `name`, `color` (theme-token slot),
  and `presence` (`camera`, `selection` of stable scene GUIDs). Pending rows have `id`, `name`
  and `address` only. No secret join nonce is exposed.
- `editor.collab_host { "action": "accept", "participant": 2 }` explicitly accepts that
  pending user. `action: "reject"` refuses the pending user. Only the local host can do either.
- `editor.collab_leave {}` closes the local socket and clears indicators immediately. Accepted
  peers learn departure on the next snapshot, or expire it after five seconds of silence if the
  leave packet was lost. Host departure ends the session. A new join always needs acceptance.
  A new join from the same address as a pending or rejected request replaces that request with a
  new pending id; an accepted participant's address ignores other join requests until it leaves.

Participant ids are local to one host session, not accounts. Theme color slots resolve through
local theme tokens, and names accompany the dots. Duplicate names are disambiguated by ids.
The Outliner and viewport resolve selection GUIDs locally; absent GUIDs draw nothing. Main
viewport camera gizmos and remote selection silhouettes share the existing **Outline** switch;
game view hides them. A participant's selection never becomes your local selection.

## Jump

`editor.collab_jump { "participant": 2 }` copies that accepted remote participant's main active
viewport camera pose and view kind to your main active viewport. It cancels that viewport's
pending camera ease and preserves your fly speed. It does not copy selection, edit entities,
change undo history, or keep following later camera movement. A departed or pending id cannot
be jumped to. Independent additional viewport windows are not published in this wave.

## Contract and limits

All five methods are editor extensions. `editor.collab_participants` needs `read`; host, join,
leave and jump need `edit_scene`. They return `applied: false` (no scene edit) and do not enter the scene change log or undo history.
Session controls are mutating calls for rate limiting; the participant query is read-only.
Read calls do not pump the network. GUI and headless loops pump it independently, at most
10 times/second (one update per 100 ms, no catch-up bursts), up to 32 incoming datagrams/update.
The host supports eight accepted participants including itself and seven pending/rejected
requests. Names are 1–64 UTF-8 bytes without control characters, selection snapshots hold at
most 64 GUIDs, and a datagram is capped at 60 KiB. Invalid, oversized, stale, wrong-session and
unaccepted presence packets are ignored. The transport is lossy: newer complete snapshots
replace older ones. There is no scene edit or arbitrary dispatch method on the LAN socket.

Error codes (`data.code`):

| Code | Meaning and recovery |
|---|---|
| `collab_bad_params` | Wrong parameter type, unknown action/field, invalid name or id. Use the schema. |
| `collab_bad_address` | Supply a numeric private, link-local or loopback IP and port; joining needs a nonzero port. |
| `collab_busy` | Already hosting/joining/joined. Leave before starting another session. |
| `collab_io` | Socket bind/setup failed. Check the address, port and firewall. |
| `collab_not_host` | Acceptance/rejection is only available in the host editor. |
| `collab_not_pending` | That id is no longer pending. Refresh the list. |
| `collab_full` | Eight accepted participants already occupy the session. |
| `collab_unknown_participant` | Jump requires a currently accepted remote id. |

Transport status errors (returned in `error`, not RPC failures): `collab_rejected`,
`collab_timeout`, `collab_host_left`, `collab_io`. Join rejection arrives on a later tick.
Full capacity is reported to the host when accepting; the joiner stays pending until a slot
opens or the host rejects it. A five-second timeout clears every stale indicator and closes the client. Each update reads
queued packets before it judges silence, so a local stall shorter than the timeout keeps the session.
Leave is idempotent. No reconnect is automatic.

## Verification

`cargo test -p regia_editor --lib collab` checks two independent worlds over a deterministic
loopback transport: admission, camera/selection convergence in three eligible ticks, a third
user pending until accepted, jump, leaving, loss, ordering, validation, bounded updates,
theme-based indicators and keyboard operation. The dispatcher integration test uses real
loopback sockets and `regia_agent` request/reply dispatch. No GPU or golden is needed.

P-333 edit replication is designed only (ADR-7842): future commands must pass through host
dispatch in sequence with atomic per-entity soft locks and revision checks. This wave builds
presence and P-334 jump only.

When selections overlap, the remote silhouette's outer ring alternates participant colors;
the local selection keeps its inner outline. This preserves both local and remote indicators.
