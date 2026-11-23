# Logs and engine events

Two polled streams, both with the same cursor pattern: keep the returned `next` and pass it as
`since` on the next call to read only what is new.

## Logs

```json
logs.read { "since": 0, "level": "info", "target": "regia_agent", "contains": "screenshot", "limit": 200 }
→ { "records": [{ "seq": 41, "unix_ms": 1730000000000, "level": "info",
                  "target": "regia_agent", "message": "…" }], "next": 42 }
```

`level` is a minimum (`error`…`trace`), `target` a prefix, `contains` a substring. The ring keeps a
few thousand records. MCP: recent records are also readable as the resource `regia://logs/recent`.

## Engine messages

`events.list` shows the tapped kinds; `events.read` polls them:

| Kind | What it is |
|---|---|
| `sim_event` | Routed sim events (`RoutedEvent`): the event kind and its mapped reactions (camera shake, toast, sound cue …) |
| `toast` | UI toasts raised by the event router |
| `settings` | Live `GameSettings` changes: the changed top-level field, before and after |
| `extension` | Extension life cycle (`ExtensionEvent`): `{ "Built": { "id", "phase" } }`, `{ "Failed": { "id", "code" } }`, and `Resolved`, `Enabled`, `Disabled`, `Reloaded`, `PackActivated`, `PackDeactivated`. See [extensions](plugins.md) |
| `spacemouse` | SpaceMouse button actions (`{ event: "action", action, button }`) and hot-plug (`{ event: "connected", id, name }`, `{ event: "disconnected", id }`); desktop only. See [SpaceMouse](spacemouse.md) |
| `procgen.run_started` | A procedural generator run started: `{ generator, entity, run_id }` (`generator` is the entity guid) |
| `procgen.run_finished` | The run ended: `{ generator, entity, run_id, ok }`; `ok: true` adds `output_hash` and `counts: { instances, entities, tables }`, `ok: false` adds the error `code` and the failing `node` (or null). Every `run_started` is followed by exactly one `run_finished` or `procgen.run_cancelled` with the same `run_id` |
| `procgen.run_cancelled` | A run was cancelled by a newer request, `procgen.cancel` or a clear |
| `procgen.bake_written` | `procgen.bake` wrote a bake: `{ generator, entity, files, cells }` (`cells` is the count of a cell re-bake, else null) |
| `procgen.cells_changed` | Resident cells of a tiled generator changed: `{ generator, entity, added, released, resident }`, cells as `[tier, i, j]` |
| `procgen.cleared` | A generator's output was removed |
| `procgen.detached` | A generator's output became level content: adds `new_root` |

Within one frame the `procgen.*` events of one generator come in run order (a cancelled run before
the run that replaced it); different generators come in entity order. See [procgen](procgen.md).

```json
events.read { "since": 0 }
→ { "records": [{ "seq": 1, "frame": 62, "unix_ms": 1730000000620, "kind": "sim_event",
                  "data": { "kind": "sim_started", "reactions": [{ "Toast": { "text": "Sim online" } }] } }],
    "next": 1 }
```

The kinds only appear when their plugin is installed (e.g. `sim_event` needs a
`SimBridgePlugin`). Use `logs_contain`/`events_contain` conditions to wait for a message inside
`play.wait_until` (see [testing](testing.md)).

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `events.list`

The engine message kinds the protocol records (see events.read).

*Read, does not mutate.*

No parameters.

Result: `{ events: [{ kind, description }] }`.

No error codes beyond the universal set.

```json
events.list {}
→ {
  "events": [
    {
      "description": "Routed sim events (regia_framework::sim_bridge::RoutedEvent): the event kind and its mapped reactions.",
      "kind": "sim_event"
    },
    {
      "description": "UI toasts raised by the event router (sim_bridge::ToastRaised).",
      "kind": "toast"
    },
    ...
  ],
  ...
}
```

Elided: `events` (array 5).

<!-- capture:events.list -->

### `events.read`

Engine messages (toasts, routed sim events, settings changes) since a cursor.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `since` | number | no | Only records newer than this sequence number. |
| `limit` | number | no | Most records to return (default 200). |

Since defaults to 0 and limit to 200. Limit 0 returns no records and keeps since as next.

Result: `{ records: [{ seq, frame, unix_ms, kind, data }], next }`.

No error codes beyond the universal set.

```json
events.read {"since": 0}
→ { "next": 1, "records": [ { "data": { "Resolved": { "count": 0, "errors": 0 } }, "frame": 2, "kind": "extension", "seq": 1, "unix_ms": 1794081905233 } ] }
```

<!-- capture:events.read -->

### `logs.read`

Recent engine log records with optional filters (poll with `since`).

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `since` | number | no | Inclusive: only records with seq >= this value. Pass the previous `next` to resume without repeats. |
| `level` | string | no | Minimum level: error, warn, info, debug, trace. |
| `target` | string | no | Log target prefix, e.g. "regia_agent". |
| `contains` | string | no | Only records whose message contains this text. |
| `limit` | number | no | Maximum records (default 200). |

`contains` is at most 1024 bytes. `target` is at most 1024 bytes.

Omitted level, target and contains apply no filter. Omitted since returns the newest limit records; a supplied since returns the oldest unread page. Limit defaults to 200, and limit 0 with a cursor leaves the cursor unchanged.

Result: `{ records: [{ seq, unix_ms, level, target, message }], next }`.

No error codes beyond the universal set.

```json
logs.read {"since": 0, "level": "info", "target": "regia_agent", "contains": "screenshot", "limit": 200}
→ { "next": 21, "records": [] }
```

<!-- capture:logs.read -->

## Workflow: poll logs and events while a game runs

1. `events.list` once, and keep the kinds the running app actually taps.
2. `logs.read` with `since` 0 and a small `limit`. Store `next`.
3. `events.read` with `since` 0. Store that `next` separately. The two cursors are not the same sequence.
4. After the game does the thing you care about (`play.step`, or just letting it run), call each read again with its own `next` as `since`.
5. Stop when a record's `message` or `kind` is the one you wanted. A kind that is not installed never appears; that is an empty `records` list, not an error.

<!-- /depth-2:method-reference -->
