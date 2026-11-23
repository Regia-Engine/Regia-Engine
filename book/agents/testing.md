# Testing gameplay: pause, step, wait_until, expect

The protocol runs the game deterministically for you: pause, step exact frames, inject input, and
verify the world with predicates (`play.wait_until`) and assertions (`play.expect`). This is how
engine milestones verify themselves.

## Deterministic play

- `play.pause` / `play.resume` — freeze or unfreeze game time (frames keep rendering).
- `play.step {"frames": 10}` — while paused, run exactly 10 frames of game time, then answer with
  the new `frame`. One step at a time (a second call while stepping answers `F0009`). At most
  10,000 frames per call; a larger count answers `F0002`. The frame the request arrives on does
  not count when it ran no game time (it was paused), so `{"frames": 1}` always moves the game one
  frame.
- `play.set_time_scale {"scale": 0.5}` — slow motion (or fast-forward) without losing determinism.
  The scale is `0..=1000`.
- `play.get_diagnostics` — fps, frame times, entity count, draw calls.

### Conditions that hold only for a few frames

The game keeps running between your calls. A `play.wait_until` checks every frame, but only the
frames after it arrives: if you press a key and then send the wait, the frames in between are
never checked, and how many there are depends on how fast your client runs. For a short window
(a jump's apex, a character crossing a stair flight), pause first, set up and press the key while
paused, then `play.step {"frames": 1}` and `play.expect` the condition after each step until it
passes or a frame budget runs out; `play.resume` when done. Every frame from the key press on is
checked, on any machine (`samples/third_person/tests/agent_expect.rs` does this for the fox on the
stairs). Each `play.step` is a mutating call: a script that steps frame by frame for longer than
about 50 frames needs a higher `rate_limit_per_second` in the project's `.regia/agent.toml`
(refusals answer `F0007`).

## Injecting input

`play.inject_input` sends raw events (to the primary window) and/or scheduled action values:

```json
{ "events": [
    { "type": "key", "key": "KeyW", "text": "w", "state": "pressed" },
    { "type": "cursor", "x": 400, "y": 300 },
    { "type": "touch", "id": 0, "phase": "started", "x": 100, "y": 200 }
] }
```

```json
{ "actions": [
    { "action": "Move", "value": [0.0, 1.0], "at_frame": 120, "duration_frames": 10 },
    { "action": "Jump", "value": 1.0 }
] }
```

Action values are numbers, `[x, y(, z)]` arrays or `{x, y, z}` objects; `at_frame` defaults to now
and `duration_frames` to 1 (action injection needs `regia_input::InputPlugin` — otherwise `F0013`).
`at_frame` is clamped to 10 000 000 frames ahead and `duration_frames` to 1 000 000. The call does
not refuse a long `events` or `actions` list.

## Conditions

`wait_until` and `expect` share one condition grammar. Entities may be ids or lookups that
re-resolve every frame (`{"name": "Player*"}`, `{"component": "Player"}`) — lookups survive entity
churn.

| Condition | Meaning |
|---|---|
| `{ "entity": e, "component": "Transform", "path": "translation.y", "op": "gte", "value": 2.0 }` | A reflected field compares against `value`. `op`: `eq`, `ne`, `gt`, `gte`, `lt`, `lte`. `path` uses reflection field names; omit it to compare the whole component |
| `{ "a": { "name": "Player" }, "b": { "name": "Door" }, "op": "within", "meters": 2.0 }` | Distance between two `Transform`s; `within` or `beyond` |
| `{ "resource": "GameSettings", "path": "input.sensitivity", "op": "eq", "value": 1.0 }` | A well-known resource: `GameSettings`, `SimCounters`, `Diagnostics`, `Time`, `QuickDraw` ([quick draw](quick-draw.md)), `QuickDraw2d` ([quick draw 2D list](quick-draw2d.md)), `Content` ([content chunks](content-chunks.md): the manager's requests as `content.progress` answers, `requests.N.state`) |
| `{ "state": "GameFlow", "op": "eq", "value": "Playing" }` | A game state's current value (`null` while it does not exist); see [game states](states.md) |
| `{ "logs_contain": "level started" }` | Any recent log record contains the text |
| `{ "events_contain": "toast" }` or `{ "events_contain": { "kind": "sim_event", "contains": "explosion" } }` | The tapped engine messages (see [events](events.md)) |
| `{ "all": [c1, c2] }`, `{ "any": [c1, c2] }`, `{ "not": c1 }` | Combinators; `all` stops at the first failure |

## wait_until

`play.wait_until { condition, timeout_ms? }`:

```json
{ "condition": { "entity": { "name": "Player" }, "component": "Transform",
                 "path": "translation.x", "op": "gte", "value": 3.5 },
  "timeout_ms": 5000 }
→ { "satisfied": true, "frame": 240, "waited_ms": 1830, "detail": "..." }
```

The condition is polled every frame; the call answers when it holds, or with
`satisfied: false` and the last evaluation detail when `timeout_ms` (default 10 s, capped at
120 s) passes. A larger `timeout_ms` waits 120 s. A
timeout is a *result*, not an error — check `satisfied`. At most 64 waits are pending at once;
one more answers `too_many_pending` (JSON-RPC -32600). Send waits with an id: a notification has no
reply, so its wait only occupies a slot.

## expect

`play.expect { assertions }` evaluates assertions **now** and reports each one:

```json
{ "assertions": [
  { "name": "discs hover at 1.5 m", "condition": { "entity": { "component": "SimId" },
      "component": "Transform", "path": "translation.y", "op": "eq", "value": 1.5 } },
  { "name": "player moved", "condition": { "entity": 7, "component": "Transform",
      "path": "translation.x", "op": "gt", "value": 5.0 } }
] }
→ { "results": [{ "name": ..., "pass": true|false, "detail": ..., "error": ...? }],
    "passed": 1, "failed": 1, "pass": false }
```

A malformed condition marks that assertion failed with an `error` (it does not fail the call).
Capture state with [seeing](seeing.md) screenshots for review evidence, and drive the frames with
`play.step` between assertions for exact reproducibility.

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `play.expect`

Evaluate assertions against the world now; returns a pass/fail report.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `assertions` | array | yes | [{ name?, condition }]; conditions as for wait_until. |

`assertions` has at most 256 items.

Result: `{ results: [{ name, pass, detail, error? }], passed, failed, pass }`.

No error codes beyond the universal set.

```json
play.expect {"assertions": [{"name": "clock runs", "condition": {"resource": "Time", "path": "frame", "op": "gte", "value": 0}}]}
→ { "failed": 0, "pass": true, "passed": 1, "results": [ { "detail": "Time.frame = 297, gte 0", "error": null, "name": "clock runs", "pass": true } ] }
```

<!-- capture:play.expect -->

### `play.get_diagnostics`

Frame rate, frame times, entity count and memory of the running app.

*Read, does not mutate.*

No parameters.

Result: with Diagnostics installed, `{ fps, frame_time_avg_ms, fps_1pct_low, entity_count, draw_calls, allocated_bytes, frames, paused }` (allocated_bytes may be null). Without that resource: `{ note, frame, elapsed_secs, paused }`. Both also carry `game_frames` (frames that ran game time; a `play.step` adds exactly one), `injected_events` and `injected_keys` (raw events `play.inject_input` wrote; ADR-7831).

No error codes beyond the universal set.

```json
play.get_diagnostics {}
→ { "allocated_bytes": null, "draw_calls": 0, "entity_count": 202, "fps": 81.60769776034202, "fps_1pct_low": 8.763480138509959, "frame_time_avg_ms": 12.253746, "frames": 165, "paused": true }
```

`paused` reports the current Time pause flag.

<!-- capture:play.get_diagnostics -->

### `play.inject_input`

Inject raw input events and/or action values (deterministic, replayable).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `events` | array | no | Raw events: { type: key/mouse_button/cursor/wheel/touch/back, ... }. |
| `actions` | array | no | { action, value, at_frame?, duration_frames? } — values: number, [x,y(,z)] or {x,y,z}. |

Result: `{ injected, scheduled, frame }`.

Errors besides the universal codes: `F0003`, `F0013`, `play_gate_text`.

A key event may set `scancode` (a physical key: a non-negative integer that fits in `u32`). The player sees `physical_key` `scancode:N` and an empty `logical_key`. A scancode together with non-empty `text` is `play_gate_text` and that batch writes nothing. A key that omits `scancode` still uses `key`. An omitted `text` on that name copies the name into `logical_key`.

```json
play.inject_input {"events": [{"type": "key", "key": "KeyW", "text": "w", "state": "pressed"}, {"type": "cursor", "x": 400, "y": 300}], "actions": [{"action": "Jump", "value": 1.0}]}
```

```json,ignore
→ { "message": "no InputInjection in this app", "code": -32004, "data": { "code": "F0013", "hint": "action injection needs regia_input::InputPlugin" } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:play.inject_input -->

### `play.pause`

Pause gameplay time (frames keep rendering).

*Play, mutating.*

No parameters.

Result: `{ paused, frame }`.

Errors besides the universal codes: `F0018`.

```json
play.pause {}
→ { "frame": 165, "paused": true }
```

<!-- capture:play.pause -->

### `play.resume`

Resume gameplay time.

*Play, mutating.*

No parameters.

Result: `{ paused, frame }`.

Errors besides the universal codes: `F0018`.

```json
play.resume {}
→ { "frame": 299, "paused": false }
```

<!-- capture:play.resume -->

### `play.set_time_scale`

Set the gameplay time scale (1 = real time, 0.5 = slow motion).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `scale` | number | yes | New scale (0..=1000). |

Result: `{ before, after }`.

Errors besides the universal codes: `F0018`.

```json
play.set_time_scale {"scale": 0.5}
→ { "after": 0.5, "before": 1.0 }
```

<!-- capture:play.set_time_scale -->

### `play.step`

While paused, run exactly `frames` frames of game time, then answer.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `frames` | number | no | Frames to run (default 1, at most 10000). |
| `timeout_ms` | number | no | Answer deadline in ms (the connection waits this long plus a 5 s grace; default 30000). |

`frames` is from 1 to 10000.

Result: `{ frame, paused }`.

Errors besides the universal codes: `F0009`, `F0018`.

```json
play.step {"frames": 10}
→ { "frame": 310, "paused": true }
```

<!-- capture:play.step -->

### `play.wait_until`

Block until a world condition holds (checked every frame) or it times out.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `condition` | object | yes | Predicate (see the guide's testing page): { all/any/not/entity/resource/state/logs_contain/events_contain }. |
| `timeout_ms` | number | no | Give up after this long (default 10000, capped at 120000). |

Result: `{ satisfied, frame, waited_ms, detail, last_error? }`.

Errors besides the universal codes: `F0014`, `too_many_pending` (both in `data.code`). At most 64 waits may be pending.

```json
play.wait_until {"condition": {"resource": "Time", "path": "frame", "op": "gte", "value": 1}, "timeout_ms": 5000}
→ { "detail": "Time.frame = 311, gte 1", "frame": 311, "satisfied": true, "waited_ms": 0 }
```

<!-- capture:play.wait_until -->

## Workflow: check one frame of gameplay

1. `play.pause`. The answer's `paused` is true and `frame` is the frame you are on.
2. `play.inject_input` with the key or action that should start the move.
3. `play.step` with `frames` 1. Wait for that answer before the next step. A second step while one is running is `F0009`.
4. `play.expect` the condition. A miss lists the failures; it does not throw the game away.
5. Repeat step and expect until the condition holds or you hit the frame budget you chose, then `play.resume`.
6. `F0007` limits mutating calls per second, not frames per call. Batch a known frame count into play.step when intermediate assertions are unnecessary, and wait for each response before submitting the next call.

<!-- /depth-2:method-reference -->
