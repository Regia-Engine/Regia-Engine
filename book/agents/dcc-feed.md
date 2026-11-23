# DCC scene feed

The DCC scene feed (spec 60) lets a content tool stream scene data into a running editor or dev game. Items appear as
ordinary entities marked feed-owned; when the artist is happy, one **bake** turns them into normal assets through the
same importer a `.glb` file uses. It is a development channel: `regia_agent` feature `dcc-feed` (dev builds only, never
in a shipping profile), plus `dcc-feed-assets` for mesh, texture and clip items (on in the editor). A build without the
feature answers `F0013`, reason `dcc_feed_disabled`; `dcc_feed.status` always answers.

## Turning it on

The feed is off until the project asks for it. `Regia.toml`:

```toml
[dcc_feed]
enabled = true
listen = ["unix", "tcp:127.0.0.1:0"]   # add "ws:127.0.0.1:0" for browser tools
auto_accept = ["my-blender-addon"]     # client names that need no prompt
playout = "snap"                       # or "delayed"
playout_delay_ms = 50
delay_queue_len = 64
max_frame_age_ms = 2000
stall_after_ms = 500
max_sessions = 4
window_bytes = 33554432
allowed_origins = []                   # Origin values a WebSocket upgrade may carry
allow_remote = false

[dcc_feed.limits]
max_vertices = 2000000
```

An absent section means off, with no migration. A non-loopback `tcp:` or `ws:` address needs `allow_remote = true`, and the
protocol has no TLS: use an SSH tunnel or a VPN. `dcc_feed.listen` opens and closes the listeners at runtime.

The server writes `.regia/dcc_feed/endpoint.json` (active transports) and `.regia/dcc_feed/token` (mode 0600; an owner-only DACL on Windows, ADR-5489), and listens on
`.regia/dcc_feed/feed.sock` (mode 0600). The Unix socket needs no token: OS permissions are the check. A `.regia/dcc_feed` that is a symbolic link (or lies in a linked `.regia`) is refused before anything is created, chmodded or removed through it (ADR-5869). TCP and WebSocket need
the token in `hello`. Authentication and approval are separate: an unknown client name waits in `pending` until
`dcc_feed.accept`, so a process that read the token still cannot change the scene unseen. `remember: true` writes the name to
`.regia/dcc_feed.toml`. Until its `hello` is accepted a session may send at most 64 KiB in one frame or message; a larger
one is `frame_too_large` and closes the session.

## Sessions and items

A session moves `negotiating` to `pending` or `active`, then `closing` and `closed`; a closed record stays 60 s for
`dcc_feed.status`. While `pending`, declarations are held and data frames are dropped and counted (`dropped_not_accepted`).
Held declarations count against the session's `window_bytes` (and at most 4096 are held); past that a declaration
is dropped with a non-fatal `overloaded` error, and the accept's `resync` asks the sender for its full state again.
Each listener serves at most 32 connections at once (negotiating ones included); a further connection is closed
before any thread starts for it.

An item is `declared`, then `active` on its first frame, `stalled` after `stall_after_ms` of feed time with no frame, and
`detached` when its session closes. A detached entity stays, frozen. A new session declaring the same key rebinds the same
entity, so anything the user attached to it survives. The item id comes from the key, the same on every machine.

The feed clock follows real time, so a paused game still shows edits. A replay steps it per tick.

## Methods

| Method | What it does |
|---|---|
| `dcc_feed.status` | Listeners, settings, sessions and counters, replay and recording progress, warnings |
| `dcc_feed.items` | Items by `session`, `kind`, `state`, `key_glob`, `limit`, with vertex, triangle, joint and channel counts |
| `dcc_feed.accept` | Accepts a pending session; `remember` allow-lists the client |
| `dcc_feed.disconnect` | Closes a session; `despawn` removes its entities. The kill switch: it always answers |
| `dcc_feed.record` | `{ action: "start", path }` or `{ action: "stop" }`; one session at a time |
| `dcc_feed.replay` | Replays a `.rfdrec` as a new session; progress in `status.replay` |
| `dcc_feed.listen` | Opens or closes the listeners |
| `dcc_feed.rotate_token` | Writes a new token; open sessions keep running |
| `dcc_feed.bake` | Editor only: bakes items to `.glb`, answers a job id |
| `dcc_feed.bake_status` | Editor only: the state of a bake job and its report |

`dcc_feed.bake` takes `{ items: [key] or "all", session?, folder, import?: { scale?, lods?, collision? }, replace_live?, dry_run? }`.
`dry_run` answers the planned files and writes nothing. Each mesh item becomes one `.glb` under `assets/<folder>/` with
vertices in object space, and the entity keeps its placement, so the bake lands exactly where the live mesh was. Transform,
camera and light items need no file: they just become ordinary entities. Skeleton and generic items have no asset form.
The same snapshot bakes to identical bytes. The `.meta` provenance has `tool: generator`, `generator: "dcc_feed"`. A later
declaration of a baked key makes a fresh feed-owned entity next to the baked one.

## Ownership

A feed-owned entity is read-only: the inspector shows a banner, gizmos do not attach, and `scene.set_property`,
`scene.add_component`, `scene.remove_component`, `scene.despawn`, `scene.duplicate`, `scene.reparent`, `scene.apply_edits`,
`editor.set_transform`, `editor.set_property` and the editor hierarchy edits answer `F0002` with reason `feed_owned`.
Selecting, framing, screenshotting and adding children stay allowed.

Save level and save prefab skip feed-owned entities. A user entity under one is saved with a `FeedAttach { key }` marker
instead of its parent link. When an item with that key exists it goes back under it, keeping its local transform; otherwise
it stays at the root and one warning per key is logged.

## Poses

A mesh item parented under a skeleton item, with joints and weights, binds to it: the feed spawns the joint entities and pose
frames move them. It draws rigid until the `skeleton_static` arrives.

A `FeedPoseTarget { item, blend, joint_map }` on a skinned entity overrides its pose by joint name after the graph and before
spring bones and the skin palette. `blend` 0 leaves the graph pose bit-identical; 1 is the stream alone. `joint_map` renames
source joints to target joints. A source joint with no target is reported once in `dcc_feed.status` `warnings`, code
`joint_unmatched`.

## Reference client

`regia dcc-feed send file.glb` streams a glTF file as an add-on would: one item per node, a skeleton per skin, textures by bytes.

```sh
regia dcc-feed send hull.glb --watch            # re-reads the file and sends what differs
regia dcc-feed send walk.glb --animate Walk --fps 30 --record take.rfdrec
regia dcc-feed record take.rfdrec && regia dcc-feed record --stop
regia dcc-feed replay take.rfdrec
regia dcc-feed dump take.rfdrec                 # one line per record
```

A skin's joints are its skeleton item. A joint is also a plain item when something hangs on it: when it owns a mesh,
camera or light, or when a non-joint node sits below it (a sword under a hand bone follows the hand). Nodes between two
joints, or above the root joint, are folded into the joint below them, in its bind, in `--animate` poses and in pushed
clips (a clip that animates such a node gives the joint below it keys at the union of the key times, exact at the keys and
linear between them). Joints are sent parents first, and a skinned mesh's `JOINTS` indices are renumbered to match.

`--endpoint tcp:127.0.0.1:7000` and `--token-file` override the project's `endpoint.json`. The byte layout for third-party
clients is `docs/formats/dcc-feed-protocol-v1.md`; the recording format is `docs/formats/rfdrec.md`.

`--watch` keeps the session's handles when a re-export renumbers the file: matching is by key. A node that moves sends one
`transform`, an edited mesh one `mesh`, a key that changes kind a delete and a new declaration; changed textures and clips are
pushed again. `--record` never writes the session token. Its `sim_tick` is one tick per clip step or `--watch` change, since the
client has no simulation, and with `--watch` the file is rewritten after each change, so stopping the client loses nothing.

`cargo run --release -p regia_cli --example dcc_feed_bench` prints the numbers of the performance table in spec 60 § 10.

## Verifying a feature

Start the client with `--record`, or call `dcc_feed.replay`. Then `wait_until` on `scene.query_entities` with `component:
"FeedItem"` and the predicate `frames_applied >= N`, `expect` on the entity's `Transform` and the vertex count in
`dcc_feed.items`, and `seeing.screenshot` the viewport. Then `dcc_feed.bake`, poll `dcc_feed.bake_status`, `save_level`, and check
the entity no longer has `FeedItem`.

No test needs a DCC. A replay injects each record at its recorded tick on a fixed clock, so one `.rfdrec` gives the same entity
state at every tick. The tests in `crates/regia_agent/src/dcc_feed/tests.rs` show each rule.

## Error codes

Wire `error.code` and agent `data.reason` share one vocabulary:
`bad_magic`, `unsupported_version`, `auth_failed`, `not_accepted`, `frame_too_large`, `limit_exceeded`,
`malformed_frame`, `unsupported_message`, `unknown_item`, `item_conflict`, `kind_mismatch`, `kind_not_supported`,
`topology_mismatch`, `invalid_value`, `path_denied`, `overloaded`, `timeout`, `internal`. A frame with a NaN or
infinite float is refused whole (`invalid_value`). `decode` answers `path_denied` for a `by path` texture no root could
accept; the apply step opens one only through `wire::resolve_source_path`, which holds it to the project and the inbox
after symlinks. `timeout` is the stream's read deadline: the transport sets it, and `FrameReader` reports it.

The agent methods add `dcc_feed_disabled`, `feed_owned`, `session_not_found`, `session_closed`, `session_negotiating`,
`already_accepted`, `record_active`, `replay_not_found`, `replay_corrupt`, `item_not_found`, `nothing_to_bake`, `not_bakeable`,
`target_exists`, `import_failed`, `bake_running` and `job_not_found`.

## Limits of this delivery

Windows has no named pipe yet: it uses loopback TCP or WebSocket with the token. Anim-graph nodes reading the pose buffer,
timecode-aligned playout, morph targets, curves, USD and bidirectional sync are follow-ups (spec 60 § 11).

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer. Absolute project paths and messages containing them are elided with `... /* comment */`.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `dcc_feed.accept`

Accepts a pending DCC feed session: held declarations apply and the sender is asked to resync. `remember` allow-lists the client name in .regia/dcc_feed.toml.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `session` | number | yes | session id from dcc_feed.status |
| `remember` | boolean | no | accept this client name without a prompt from now on |

`remember` defaults to false.

Result: `{ accepted, resync_sent, remembered }`.

Errors: `F0002`, `F0013`. `data.reason`: `dcc_feed_disabled`, `session_not_found`, `already_accepted`, `session_closed`, `session_negotiating`, `internal`. `F0013` is the build without DCC feed support; `F0002` is a handler refusal.

```json
dcc_feed.accept {"session": 1, "remember": true}
```

```json,ignore
→ { "message": "session 1: session_not_found", "code": -32602, "data": { "code": "F0002", "hint": "see book/agents/dcc-feed.md for this reason", "reason": "session_not_found" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:dcc_feed.accept -->

### `dcc_feed.bake`

Bakes feed items into ordinary .glb assets through the normal importer. Answers a job id; dcc_feed.bake_status yields the report. `dry_run` answers the planned files and writes nothing. The editor provides it; every other build answers F0013.

*Assets, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `items` | any | yes | item keys, or "all" |
| `session` | number | no | with "all": only this session's items |
| `folder` | string | yes | destination folder under assets/ |
| `import` | object | no | { scale?, lods?, collision? }. A present forward or generate_tangents is a warning and is not applied. |
| `replace_live` | boolean | no | make the entities ordinary content using the assets (default true) |
| `dry_run` | boolean | no | answer the planned files; write nothing |

`folder` is at most 4096 bytes.

`dry_run` defaults to false. With items `"all"`, an omitted session includes every session. Omitted import overrides preserve object scale/normal importer LOD selection and use collision false. `scale` is a positive number, `lods` a u32 and `collision` a boolean. The handler reports warnings for `forward` and `generate_tangents`: files keep object space and tangent generation follows material needs.

Result: `{ job, files, plain, warnings } or, with dry_run, { dry_run, files, plain, warnings }`.

Errors: `F0002`, `F0013`. `data.reason`: `dcc_feed_disabled`, `item_not_found`, `nothing_to_bake`, `not_bakeable`, `target_exists`, `invalid_value` (the feed data is inconsistent: a short attribute array, an index or draw range past the arrays, mismatched skin arrays, or a non-finite import scale), `path_denied`, `bake_running`, `internal`. `F0013` means the feature or editor handler is unavailable.

```json
dcc_feed.bake {"items": "all", "folder": "live"}
```

```json,ignore
→ { "message": "nothing to bake", "code": -32602, "data": { "code": "F0002", "hint": "see book/agents/dcc-feed.md for this reason", "reason": "nothing_to_bake" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:dcc_feed.bake -->

### `dcc_feed.bake_status`

The state of a bake job from dcc_feed.bake. The editor frame applies a finished worker; this call only reports it. When the job is done, the reply is its report. The editor provides it; every other build answers F0013.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `job` | number | yes | the job id |

Result: `{ job, state: running | done | failed, assets, glb_paths, converted, warnings, reason?, error? }`.

Errors: `F0002`, `F0013`. `data.reason`: `job_not_found`. `F0013` means the feature or editor handler is unavailable.

```json
dcc_feed.bake_status {"job": 1}
```

```json,ignore
→ { "message": "no bake job 1", "code": -32602, "data": { "code": "F0002", "hint": "see book/agents/dcc-feed.md for this reason", "reason": "job_not_found" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:dcc_feed.bake_status -->

### `dcc_feed.disconnect`

Closes a DCC feed session. Its items stay as detached entities unless `despawn` is true. The kill switch of a runaway stream: always answers.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `session` | number | yes | session id from dcc_feed.status |
| `despawn` | boolean | no | also remove the session's entities (default false) |

Result: `{ closed, items_detached, despawned }`.

Errors: `F0002`, `F0013`. `data.reason`: `dcc_feed_disabled`, `session_not_found`. `F0013` is the build without DCC feed support; `F0002` is a handler refusal.

```json
dcc_feed.disconnect {"session": 1, "despawn": false}
```

```json,ignore
→ { "message": "session 1: session_not_found", "code": -32602, "data": { "code": "F0002", "hint": "see book/agents/dcc-feed.md for this reason", "reason": "session_not_found" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:dcc_feed.disconnect -->

### `dcc_feed.items`

Items the DCC feed streams: key, stable guid, entity, kind, state, frame counters and size details (vertices, triangles, joints, channels). Filters combine.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `session` | number | no | only items of this session (session_not_found when there is none) |
| `kind` | string | no | transform, mesh, camera, light, skeleton or generic |
| `state` | string | no | declared, active, stalled or detached |
| `key_glob` | string | no | whole-key pattern with * and ? |
| `limit` | number | no | most items (default 1000) |

`key_glob` is at most 256 bytes.

Omitted `session`, `kind`, `state` and `key_glob` apply no filter; limit defaults to 1000.

Result: `{ items: [{ key, guid, handle, entity, kind, state, owned, session, frames_applied, last_frame_id, last_source_time_ns, dropped_out_of_order, detail: { vertices, triangles, joints, channels } }] }`.

Errors: `F0002`, `F0013`. `data.reason`: `dcc_feed_disabled`, `session_not_found`. `F0013` is the build without DCC feed support; `F0002` is a handler refusal.

```json
dcc_feed.items {"kind": "mesh", "limit": 100}
→ { "items": [] }
```

<!-- capture:dcc_feed.items -->

### `dcc_feed.listen`

Opens or closes the DCC feed listeners; closing cuts every connection. The editor panel's toggle.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `enabled` | boolean | yes | true opens the listeners of [dcc_feed], false closes them |

Result: `{ enabled, listeners: [{ transport, address }] }`.

Errors: `F0002`, `F0013`. `data.reason`: `dcc_feed_disabled`, `internal`. `F0013` is the build without DCC feed support; `F0002` is a handler refusal.

```json
dcc_feed.listen {"enabled": true}
→ {
  "enabled": true,
  "listeners": [
    {
      "address": ... /* capture project path or message containing it */,
      "transport": "unix"
    },
    {
      "address": "127.0.0.1:43007",
      "transport": "tcp"
    }
  ]
}
```

<!-- capture:dcc_feed.listen -->

### `dcc_feed.record`

Starts or stops recording one DCC feed session to a .rfdrec file (relative to the project). A start asks the sender to resync, so the file stands on its own.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `action` | string | yes | "start" or "stop" |
| `path` | string | no | the .rfdrec file, relative to the project (required to start) |
| `session` | number | no | the session to record (default: the first active one) |

`path` is at most 4096 bytes.

Start requires a path; an omitted session records all sessions. Stop uses the active recording and does not need a path. Stopping without a recording returns `invalid_argument` in `data.reason`.

Result: `{ recording, path, records }`.

Errors: `F0002`, `F0013`. `data.reason`: `dcc_feed_disabled`, `record_active`, `session_not_found`, `path_denied`, `internal`, `invalid_argument`. `F0013` is the build without DCC feed support; `F0002` is a handler refusal.

```json
dcc_feed.record {"action": "start", "path": "takes/one.rfdrec"}
→ { "path": ... /* capture project path or message containing it */, "recording": true, "records": 0 }
```

<!-- capture:dcc_feed.record -->

### `dcc_feed.replay`

Replays a .rfdrec as a new session on the fixed feed clock: the same file gives the same entity state at every tick. Progress is in dcc_feed.status `replay`.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | yes | the .rfdrec file, relative to the project |
| `ticks_per_record` | number | no | fixed ticks between records (default: the recorded spacing) |
| `loop` | boolean | no | restart at the end |
| `as_client` | string | no | client name of the replay session (an unknown name waits in pending) |

`path` is at most 4096 bytes.

`loop` defaults to false. Omitted ticks_per_record replays recorded timing; an omitted as_client uses the recorded client identity.

Result: `{ session, records_total }`.

Errors: `F0002`, `F0013`. `data.reason`: `dcc_feed_disabled`, `replay_not_found`, `replay_corrupt`, `record_active`, `path_denied`. `F0013` is the build without DCC feed support; `F0002` is a handler refusal.

```json
dcc_feed.replay {"path": "takes/one.rfdrec", "loop": false}
```

```json,ignore
→ {
  "message": "io: No such file or directory (os error 2)",
  "code": -32602,
  "data": {
    "code": "F0002",
    "hint": "see book/agents/dcc-feed.md for this reason",
    "reason": "replay_not_found"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:dcc_feed.replay -->

### `dcc_feed.rotate_token`

Replaces the DCC feed token in .regia/dcc_feed/token. Open sessions keep running; new TCP senders need the new token.

*EditScene, mutating.*

No parameters.

Result: `{ rotated }`.

Errors: `F0002`, `F0013`. `data.reason`: `dcc_feed_disabled`, `internal`. `F0013` is the build without DCC feed support; `F0002` is a handler refusal.

```json
dcc_feed.rotate_token {}
→ { "rotated": true }
```

<!-- capture:dcc_feed.rotate_token -->

### `dcc_feed.status`

The DCC scene feed: listeners, settings and every session (state, client, items, byte and frame counters, last error), replay and recording progress, warnings. Answers in every build.

*Read, does not mutate.*

No parameters.

Result: `{ enabled, listeners: [{ transport, address }], settings, sessions: [{ id, client, client_version, dcc, state, transport, peer, protocol_version, items, bytes_in, frames_in, frames_applied, frames_coalesced, dropped, dropped_not_accepted, last_error, close_reason }], replay, recording, warnings, transient_textures, transient_clips }`.

No error codes beyond the universal set.

```json
dcc_feed.status {}
→ {
  "enabled": false,
  "listeners": [],
  "recording": null,
  "replay": null,
  "sessions": [],
  "settings": {
    "allow_remote": false,
    "allowed_origins": [],
    "auto_accept": [],
    "delay_queue_len": 64,
    "enabled": false,
    "limits": {
      "max_indices": null,
      "max_items": null,
      "max_joints": null,
      "max_key_len": null,
      "max_message_bytes": null,
      "max_texture_dim": null,
      "max_vertices": null
    },
    "listen": [
      "unix",
      "tcp:127.0.0.1:0"
    ],
    "max_frame_age_ms": 2000,
    "max_sessions": 4,
    "playout": "snap",
    "playout_delay_ms": 50,
    "stall_after_ms": 500,
    "window_bytes": 33554432
  },
  "transient_clips": 0,
  "transient_textures": 0,
  "warnings": []
}
```

<!-- capture:dcc_feed.status -->

## Workflow: accept a client and close it

1. `dcc_feed.status`. A build without the feature, or a world with no `DccFeedSettings`, succeeds with `enabled: false`, `settings: null` and `reason: dcc_feed_disabled`. Other feed calls then return `F0013`. The example above is a live feed whose settings resource exists. An empty sessions list means nothing is connected.
2. `dcc_feed.listen` with `enabled` true when the project has not turned the feed on.
3. When a session sits pending, `dcc_feed.accept` with its `session` id. `remember` true writes the client name into the allow list. `session_not_found`, `already_accepted`, `session_closed` and `session_negotiating` are `F0002` reasons.
4. `dcc_feed.items` to read what that session declared.
5. `dcc_feed.disconnect` when the client should drop. `despawn` true removes the feed entities too.

<!-- /depth-2:method-reference -->

`regia dcc-feed send --animate` rejects a nonfinite or negative clip duration and more than 16,777,216 sample steps with `animation_samples_invalid` before its paced animation loop. The bound keeps every integer index exactly representable by the `f32` sampling clock. Split or trim an oversized clip; STEP tracks use the new key at an exact interior key time.

DCC texture pushes resolve percent-encoded external image paths and base64 `data:` image URIs through the same decoders as the asset importer. The image declaration supplies the MIME type when present; embedded MIME headers and decoded file extensions provide the defaults.
