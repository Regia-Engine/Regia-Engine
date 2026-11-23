# Demo recording and spectator camera

`regia_framework::demo` records an ECS game into a self-describing `.regiademo` file with periodic
keyframes, and seeks to any tick by restoring the nearest keyframe and replaying the recorded inputs.
`regia_framework::spectator` is a pure-logic camera (free-fly, follow with smoothing, speed control) for
watching one. It builds on `regia_framework::input_recording`: `DemoRecording::from_input_recording` wraps what
an `InputRecorder` captured, and `to_input_recording` hands the stream back to `InputRecorder::play`.

## The file

RON through serde, `format_version` 2, and a last line `// regia-demo-end`
([format page](../../docs/formats/regiademo.md)). The header holds
`engine_version`, `build_hash`, `content_hash`, `seed`, `level`, `tick_rate_hz`, `step_count` and
`keyframe_interval`. After it come the tracked `actions`, one row of `[x, y, z]` values per fixed step, and the
`keyframes`: `tick`, `state_hash` (the state *before* step `tick` runs) and `snapshot_hex` (the game's
snapshot bytes, empty for a hash-only keyframe). Keyframe ticks strictly ascend. Since version 2, `timestamps`
keeps the input timestamps `InputRecorder` captured: per step, the arrival time of each raw input event in
microseconds since recording started (analysis data; a replay never reads them). Version 1 files still load, with
no timestamps, and `regia upgrade-project --write` rewrites them as version 2. `DemoRecording::save` writes through
a temp file and a rename.

The same record has a binary form, the `.frec` match record ([format page](../../docs/formats/frec.md)):
`RFRC` magic, version, payload length, a postcard payload and a CRC-32. `input_recording::save_frec` /
`load_frec` write and read it, `InputRecording::to_frec` writes one from a recorder's stream, and
`DemoRecording::load_any` reads either form. A damaged `.frec` is `corrupt` (bad magic or checksum) or
`truncated` (shorter than its declared payload).

A game plugs its simulation in through `DemoSim` (`step`, `state_hash`, `snapshot`, `restore`), or lets
`regia_framework::demo_world` do it for the whole world (next section). For a reflected world
`demo::world_state_hash` hashes every reflected component, so a world with frame-rate writes (a camera `Transform`) wants `demo::world_sim_state_hash(world, keep)`, which hashes the components you name. `DemoRecorder` runs the sim while recording and
adds a keyframe every `keyframe_interval` steps, plus ticks 0 and the end.

`DemoRecording::seek(sim, tick)` restores the nearest snapshot keyframe at or before `tick`, compares its hash,
replays the inputs, and compares every keyframe it passes. A different build, a changed content file or a
nondeterministic system shows up as `diverged` at the first keyframe where it happens. `verify` does the same
from the first snapshot to the end. `DemoHeader::mismatches` names the identity fields that differ from the
running build.

## Recording and scrubbing a running game

`demo_world` implements `DemoSim` for an ECS world, so a game does not write snapshot code:

```rust,ignore
use regia_framework::demo_world::{DemoTracked, DemoWorldPlugin, RegisterDemoResourceExt};
use regia_framework::demo_scrub::DemoScrubPlugin;
use regia_framework::spectator::{SpectatorPlugin, SpectatorView};

app.add_plugins((DemoWorldPlugin, DemoScrubPlugin, SpectatorPlugin))
    .register_demo_resource::<MatchState>();   // a resource that is simulation state
enable_fixed_step(&mut app, 60.0);              // one fixed step per frame: exact seeks
commands.spawn((DemoTracked, Name::new("hero"), Mover::default()));
commands.spawn((Camera::default(), Transform::default(), SpectatorView));
```

- **What is tracked**: every entity with `DemoTracked` (its reflected components, minus the short type names in
  `DemoWorldConfig::excluded_components`; `GlobalTransform`, `PreviousGlobalTransform` and `Children` are excluded by
  default) and every resource registered with `register_demo_resource`. Keyframe hashes use the canonical state
  text (entities keyed by `Name`, never by id, also where a resource refers to one); snapshots store each component as typed postcard.
- **Restore**: tracked entities that still exist keep their ids; ones spawned since the keyframe are despawned;
  ones despawned since come back as new entities (a playback in a fresh session is the usual case), and entity references inside restored components and resources follow them
  when the type registers `ReflectMapEntities`. Tracked resources added since are removed. A snapshot that cannot
  be read changes nothing.
- **Seek**: restore the nearest snapshot keyframe, then run `First`, `PreUpdate` and `FixedUpdate` once per
  recorded step with the recorded actions injected. Keep the simulation in `FixedUpdate`. A seek costs up to
  `keyframe_interval - 1` fixed steps in one frame. Agent requests wait for the end of the catch-up, and
  `DemoCatchUp` is in the world during it, for a game system that must not repeat a sound or a hit marker.
- **Not restored**: clocks, the evaluated action phases of the frame before the keyframe, untracked entities. A
  simulation that reads them diverges after a seek; the next keyframe check reports `diverged` (and the live
  driver pauses there) instead of showing a wrong state. A seek also rebuilds a parent's `Children` in entity-id
  order, so a simulation that depends on that order should sort them.

`DemoDriver` (from `DemoWorldPlugin`) is the live driver: `request(DemoRequest::Record { .. } | StopRecording
{ path } | Play(demo) | Pause(bool) | Seek(tick) | Unload)`. Requests apply in `Last` of the same frame.
Pausing sets `Time::paused`, so no fixed step runs. The scrub bar (`DemoScrubBar::open`) is a strip at the
bottom of the screen: a play/pause button, a slider over the steps with a mark under each keyframe, and a
`time / total  tick t/n  mode` label; dragging seeks.

`SpectatorPlugin` drives the `SpectatorCamera` every frame from `SpectatorDrive` (`input`, and `follow`: an
entity whose position the follow mode chases) using wall-clock time, so it moves while a replay is paused, and
writes the pose to the `Transform` of every `SpectatorView` entity.

## `demo.status`, `demo.record_start`, `demo.record_stop`

These and the next methods need `DemoWorldPlugin` (else `demo_not_installed`). Requests are queued and applied
at the end of the frame: read the result with `demo.status` after it (or `wait_until` on it).

`demo.status` (read-only) returns `mode` (`idle`, `recording`, `playing`, `paused`, `finished`), `tick`,
`steps`, `tick_rate_hz`, `keyframes` (ticks, capped at 200 with `keyframes_truncated`), `pending`,
`diverged_at`, `last_error` (`{code, message}`; codes `demo_diverged`, `demo_restore_failed`, `demo_busy`,
`demo_no_demo`, `demo_invalid` and the file codes), `last_seek` (`tick`, `keyframe_tick`, `steps_replayed`,
`keyframes_checked`), `saved_to` and `scrub_bar_open`.

`demo.record_start` takes optional `level`, `seed`, `tick_rate_hz` (1..=10000, default 60),
`keyframe_interval` (default 60; 0 keeps only the start and end), `build_hash`, `content_hash` and `actions`
(default: every action of the input config, sorted). `demo_busy` while already recording.
`demo.record_stop` takes an optional `path`: a project-relative `.frec` or `.regiademo` (anything else, an
absolute path or `..` is `demo_bad_path`); `demo_no_demo` when not recording. The file is written under the
project root, not the working directory, and a folder on the way that is a symbolic link leading out of the
project is `F0002` (`path_outside_project`); `demo.status` `saved_to` answers the path project-relative (ADR-5869).

```json
{ "method": "demo.record_start", "params": { "level": "arena", "seed": 42, "keyframe_interval": 60 } }
{ "method": "demo.record_stop", "params": { "path": "saved/demos/run-001.frec" } }
```

## `demo.play`, `demo.pause`, `demo.seek`, `demo.scrub_bar`

`demo.play` loads `path` (`.frec` or `.regiademo`; the file error codes apply) or, without one, the last
recording, restores the first keyframe and plays; it returns `steps`. `demo.pause` takes `paused` (default
true). `demo.seek` takes `tick` and answers the plan (`keyframe_tick`, `steps_to_replay`) at once; the world
reaches the tick at the end of the frame and keeps its paused state. Errors: `demo_no_demo`, `demo_busy`
(recording), `demo_tick_out_of_range`, `demo_no_keyframe`. `demo.scrub_bar` takes `open` (default true) and needs
`DemoScrubPlugin`.

```json
{ "method": "demo.play", "params": { "path": "saved/demos/run-001.frec" } }
{ "method": "demo.seek", "params": { "tick": 600 } }
{ "method": "demo.status", "params": {} }
```

## `demo.inspect`

Params: `path` (required, `.regiademo` or `.frec`). Needs the `assets` permission. Reads the file (at most 256 MiB, regular files only) and never
runs it. `path` is confined like `render.out_start`'s `out` (ADR-1769): relative to the project root, or absolute under
the project root, the system temp dir or an `output_roots` entry of `.regia/agent.toml`; `..` and a
symlink leading out of them are `F0002` before anything is read. `demo.seek_plan` takes the same `path`.

```json
{ "method": "demo.inspect", "params": { "path": "saved/demos/arena-001.regiademo" } }
```

Returns `header`, `engine_matches` and `mismatches[]` (header fields that differ from this build; an empty
`build_hash` or `content_hash` means unknown), `actions[]`, `steps`, `duration_secs`, `keyframe_count`,
`keyframes[]` (`tick`, `state_hash` as 16 hex digits, `snapshot_bytes`; capped at 200 with
`keyframes_truncated`) and `has_timestamps`.

Errors (`INVALID_PARAMS`; `data.code` is one of): `demo_truncated` (no end marker: a cut-off file), `demo_parse`
(not valid RON for a demo), `demo_unsupported_version` (a `format_version` this build does not read),
`demo_invalid` (it parses but contradicts itself: `step_count` differs from the rows, a row of the wrong width or
with a non-finite value, a keyframe past the end or out of order, bad snapshot hex, timestamp rows that do not
match the steps), `demo_corrupt` (a `.frec` with a bad magic or checksum), `demo_too_large`, `demo_io`.

## `demo.seek_plan`

Params: `path`, `tick` (0 up to the step count). Needs `assets`. Returns `keyframe_tick`, `steps_to_replay`,
`state_hash` and `exact_keyframe`. Extra errors: `demo_tick_out_of_range`, `demo_no_keyframe` (no snapshot keyframe at or
before the tick).

## `spectator.get`, `spectator.set`, `spectator.step`

These drive the world's `SpectatorCamera` resource, created at the origin on first use. `get` is read-only;
`set` and `step` need `play`. All three return `mode` (`free_fly` or `follow`), `position`, `rotation` as
`[x, y, z, w]`, `yaw`, `pitch`, `speed_level`, `speed`, `follow_distance`, `anchor` and `target`.

`spectator.set` takes optional `position`, `yaw`, `pitch` (clamped below 90 degrees), `speed_level` (clamped to
12 either way; speed is `8 m/s * 1.25^level`), `mode`, `target` and `clear_target`. `spectator.step` takes
`frames` (default 1, at most 100000), `dt` (default 1/60; over 0.25 s is clamped), `movement` as
`[right, up, forward]`, `look` as `[yaw, pitch]` per frame, and one-shot `speed_steps` and `zoom`.

```json
{ "method": "spectator.set", "params": { "position": [0, 5, 10], "mode": "follow", "target": [0, 1, 0] } }
{ "method": "spectator.step", "params": { "frames": 120, "dt": 0.016 } }
```

Free-fly moves along the view's right and forward axes and world up; diagonals are normalized. Follow eases an
anchor toward the target by `1 - exp(-dt / 0.2 s)` per frame (frame-rate independent) and orbits it at
`follow_distance` (1 to 60 m). Non-finite numbers count as zero and a non-positive `dt` does nothing. Bad
parameter types, and numbers too large for 32-bit floats (`1e300`), are `F0002`.

## `spectator.bind`

Needs `play`. Marks a render camera `SpectatorView` (param `camera`: an entity id, `{"name": ...}` or
`{"component": ...}`; default the first entity with a `Camera`) and adds a `Transform` if it has none.
`follow` (same reference forms) supplies that entity's world pose (switching FreeFly to Follow,
while preserving Orbit, POV and Overhead; see [spectator modes](spectator-modes.md));
`clear_follow` stops. If the followed entity disappears the follow stops, and resumes when a `demo.seek` brings it
back under a new id (`DemoRemap`). Returns the camera state plus `camera`, `follow` and `plugin_installed`.
`plugin_installed` is true when a `SpectatorDrive` resource was already in the world at the start of the call.
The first call on a world without one returns false and inserts `SpectatorDrive` (and `SpectatorView` on the
camera); a later call returns true because that resource now exists. The flag does not mean `SpectatorPlugin`
registered `drive_spectator`: the bound camera's `Transform` follows the drive only after the plugin is added.
`spectator.step` updates the `SpectatorCamera` resource and does not write `SpectatorView`. An unresolvable
reference is `spectator_bad_entity`.

```json
{ "method": "spectator.bind", "params": { "camera": { "name": "MainCamera" }, "follow": { "name": "Hero" } } }
```

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `demo.inspect`

Read a demo file's header (engine version, build and content hashes, seed, level, tick rate), input row count, tracked actions and keyframe index; reports which header fields differ from this build. Reads .regiademo and .frec.

*Assets, does not mutate, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | yes | The .regiademo or .frec file. |

Result: `{ path, header, engine_matches, mismatches, actions, steps, duration_secs, keyframe_count, keyframes: [{ tick, state_hash, snapshot_bytes }], keyframes_truncated, has_timestamps }`.

Errors besides the universal codes (`data.code`): `demo_corrupt`, `demo_invalid`, `demo_io`, `demo_parse`, `demo_too_large`, `demo_truncated`, `demo_unsupported_version`. Path refusals use `F0002` with `data.reason: path_outside_project` (and `data.path_reason` for the specific path rule).

```json
demo.inspect {"path": "saved/demos/arena-001.regiademo"}
```

```json,ignore
→ { "message": "demo.inspect: demo file IO: No such file or directory (os error 2)", "code": -32602, "data": { "code": "demo_io", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 124 chars).

<!-- capture:demo.inspect -->

### `demo.pause`

Pause or resume the loaded demo (pausing stops fixed steps; the spectator camera still moves).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `paused` | boolean | no | true pauses (default), false resumes. |

Result: `{ queued, pending }`.

Errors besides the universal codes (`data.code`): `demo_busy`, `demo_no_demo`, `demo_not_installed`.

```json
demo.pause {"paused": true}
```

```json,ignore
→ {
  "message": "demo.pause: no DemoDriver in this world",
  "code": -32001,
  "data": {
    "code": "demo_not_installed",
    "hint": "add regia_framework::demo_world::DemoWorldPlugin to the app (and DemoScrubPlugin for the scrub bar)"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:demo.pause -->

### `demo.play`

Load a demo file (.frec or .regiademo; default the last recording) and play it from tick 0: the world is restored to the first keyframe, then replays the recorded inputs, checking each keyframe hash it reaches.

*Play, mutating, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | no | The demo file; omit to replay the last recording. |

An omitted path replays the last in-memory recording; without one the result is `demo_no_demo`.

Result: `{ queued, pending, steps }`.

Errors besides the universal codes (`data.code`): `demo_busy`, `demo_corrupt`, `demo_invalid`, `demo_io`, `demo_no_demo`, `demo_not_installed`, `demo_parse`, `demo_too_large`, `demo_truncated`, `demo_unsupported_version`. Path refusals use `F0002` with `data.reason: path_outside_project` (and `data.path_reason` for the specific path rule).

```json
demo.play {"path": "saved/demos/run-001.frec"}
```

```json,ignore
→ {
  "message": "demo.play: no DemoDriver in this world",
  "code": -32001,
  "data": {
    "code": "demo_not_installed",
    "hint": "add regia_framework::demo_world::DemoWorldPlugin to the app (and DemoScrubPlugin for the scrub bar)"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:demo.play -->

### `demo.record_start`

Start recording the running game: input rows per fixed step plus a keyframe (state hash and world snapshot of DemoTracked entities and demo resources) every keyframe_interval steps. Applied at the end of this frame.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `level` | string | no | Level name for the header. |
| `seed` | number | no | RNG seed for the header (the game must seed with it). |
| `tick_rate_hz` | number | no | Fixed steps per second, 1..=10000. Default 60. |
| `keyframe_interval` | number | no | Steps between keyframes (0: start and end only). Default 60. |
| `build_hash` | string | no | Game build identifier for the header. |
| `content_hash` | string | no | Content hash for the header. |
| `actions` | array | no | Action names to record; default every action of the InputConfig, sorted. |

`tick_rate_hz` is from 1 to 10000. `keyframe_interval` is from 0 to 4294967295.

Result: `{ queued, pending }`.

Errors besides the universal codes (`data.code`): `demo_busy`, `demo_not_installed`.

```json
demo.record_start {"level": "arena", "seed": 42, "tick_rate_hz": 60, "keyframe_interval": 60}
```

```json,ignore
→ {
  "message": "demo.record_start: no DemoDriver in this world",
  "code": -32001,
  "data": {
    "code": "demo_not_installed",
    "hint": "add regia_framework::demo_world::DemoWorldPlugin to the app (and DemoScrubPlugin for the scrub bar)"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:demo.record_start -->

### `demo.record_stop`

Stop recording (adds the final keyframe); with path, save the demo as a binary .frec or a RON .regiademo by extension. demo.status saved_to confirms the write.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | no | Project-relative .frec or .regiademo path (no `..`). |

An omitted path stops into memory without saving a file. A supplied relative path must contain no parent traversal.

Result: `{ queued, pending }`.

Errors besides the universal codes (`data.code`): `demo_bad_path`, `demo_no_demo`, `demo_not_installed`.

```json
demo.record_stop {"path": "saved/demos/run-001.frec"}
```

```json,ignore
→ {
  "message": "demo.record_stop: no DemoDriver in this world",
  "code": -32001,
  "data": {
    "code": "demo_not_installed",
    "hint": "add regia_framework::demo_world::DemoWorldPlugin to the app (and DemoScrubPlugin for the scrub bar)"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:demo.record_stop -->

### `demo.scrub_bar`

Show or hide the runtime demo scrub bar (play/pause button, a seek slider with keyframe marks, time and tick label).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `open` | boolean | no | true shows (default), false hides. |

Result: `{ open }`.

Errors besides the universal codes (`data.code`): `demo_not_installed`.

```json
demo.scrub_bar {"open": true}
```

```json,ignore
→ {
  "message": "demo.scrub_bar: no DemoScrubBar in this world",
  "code": -32001,
  "data": {
    "code": "demo_not_installed",
    "hint": "add regia_framework::demo_scrub::DemoScrubPlugin (with DemoWorldPlugin and the runtime UI)"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:demo.scrub_bar -->

### `demo.seek`

Put the world at a tick of the loaded demo: restore the nearest snapshot keyframe, replay recorded inputs to the tick and check every keyframe hash passed. Keeps the paused state; applied at the end of this frame.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `tick` | number | yes | Target tick, 0 up to the demo's step count. |

`tick` is from 0 to 4294967295.

Result: `{ queued, pending, tick, keyframe_tick, steps_to_replay }`.

Errors besides the universal codes (`data.code`): `demo_busy`, `demo_no_demo`, `demo_no_keyframe`, `demo_not_installed`, `demo_tick_out_of_range`.

```json
demo.seek {"tick": 600}
```

```json,ignore
→ {
  "message": "demo.seek: no DemoDriver in this world",
  "code": -32001,
  "data": {
    "code": "demo_not_installed",
    "hint": "add regia_framework::demo_world::DemoWorldPlugin to the app (and DemoScrubPlugin for the scrub bar)"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:demo.seek -->

### `demo.seek_plan`

Which snapshot keyframe a seek to a tick restores and how many recorded steps it then replays (the demo is not run).

*Assets, does not mutate, heavy.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `path` | string | yes | The .regiademo file. |
| `tick` | number | yes | Target tick, 0 up to the demo's step count. |

`tick` is from 0 to 4294967295.

Result: `{ tick, keyframe_tick, steps_to_replay, state_hash, exact_keyframe }`.

Errors besides the universal codes (`data.code`): `demo_corrupt`, `demo_invalid`, `demo_io`, `demo_no_keyframe`, `demo_parse`, `demo_tick_out_of_range`, `demo_too_large`, `demo_truncated`, `demo_unsupported_version`. Path refusals use `F0002` with `data.reason: path_outside_project` (and `data.path_reason` for the specific path rule).

```json
demo.seek_plan {"path": "saved/demos/arena-001.regiademo", "tick": 600}
```

```json,ignore
→ { "message": "demo.seek_plan: demo file IO: No such file or directory (os error 2)", "code": -32602, "data": { "code": "demo_io", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 124 chars).

<!-- capture:demo.seek_plan -->

### `demo.status`

The live demo driver: mode (idle, recording, playing, paused, finished), playhead tick, step count, keyframe ticks, the last seek, the first diverged keyframe and the last error.

*Read, does not mutate.*

No parameters.

Result: `{ mode, tick, steps, tick_rate_hz, keyframes, keyframes_truncated, pending, diverged_at, last_error: { code, message }?, last_seek: { tick, keyframe_tick, steps_replayed, keyframes_checked }?, saved_to, scrub_bar_open }`.

Errors besides the universal codes (`data.code`): `demo_not_installed`.

```json
demo.status {}
```

```json,ignore
→ {
  "message": "demo.status: no DemoDriver in this world",
  "code": -32001,
  "data": {
    "code": "demo_not_installed",
    "hint": "add regia_framework::demo_world::DemoWorldPlugin to the app (and DemoScrubPlugin for the scrub bar)"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:demo.status -->

### `spectator.bind`

Drive a render camera from the spectator camera (marks it SpectatorView; SpectatorPlugin writes the pose each frame) and optionally follow an entity.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `camera` | any | no | Entity reference (id, {name} or {component}); default the first entity with a Camera. |
| `follow` | any | no | Entity reference to follow (switches to follow mode). |
| `clear_follow` | boolean | no | Stop following an entity. |

Omitted camera chooses the Camera entity with the lowest id. Omitted follow keeps the current follow binding; `clear_follow` defaults to false. A supplied follow switches to follow mode.

Result: `{ camera, follow, plugin_installed, mode, position, rotation, yaw, pitch, speed_level, speed, follow_distance, anchor, target }`.

Errors besides the universal codes (`data.code`): `spectator_bad_entity`.

```json
spectator.bind {"camera": {"name": "MainCamera"}, "follow": {"name": "Hero"}}
```

```json,ignore
→ {
  "message": "spectator.bind: camera: no entity matches {\"name\":\"MainCamera\"} — use scene.query_entities to see what exists",
  "code": -32001,
  "data": {
    "code": "spectator_bad_entity",
    "hint": "pass an entity id, {\"name\": \"...\"} or {\"component\": \"...\"} (scene.query_entities lists them)"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:spectator.bind -->

### `spectator.get`

The spectator camera (created at the origin when absent): mode, pose, speed level, follow anchor and target.

*Read, does not mutate.*

No parameters.

Result: `{ mode, position, rotation, yaw, pitch, speed_level, speed, follow_distance, anchor, target }`.

No error codes beyond the universal set.

```json
spectator.get {}
→ {
  "anchor": [
    0.0,
    0.0,
    0.0
  ],
  "follow_distance": 6.0,
  "mode": "free_fly",
  "pitch": 0.0,
  "position": [
    0.0,
    0.0,
    0.0
  ],
  "rotation": [
    0.0,
    0.0,
    0.0,
    1.0
  ],
  "speed": 8.0,
  "speed_level": 0,
  "target": null,
  "yaw": 0.0
}
```

<!-- capture:spectator.get -->

### `spectator.set`

Set the spectator camera's pose, speed level, mode (free_fly, follow, orbit, pov or overhead) and follow target; every parameter is optional.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `position` | array | no | [x, y, z] meters. |
| `yaw` | number | no | Radians; positive turns left. |
| `pitch` | number | no | Radians, clamped below 90 degrees. |
| `speed_level` | number | no | Integer level, clamped to +-12; speed is base * 1.25^level. |
| `mode` | string | no | "free_fly" or "follow". |
| `target` | array | no | [x, y, z] point follow mode chases. |
| `clear_target` | boolean | no | Stop chasing; follow holds its anchor. |

`speed_level` is from -2147483648 to 2147483647.

Omitted pose, speed, mode and target fields retain the current values. `clear_target` defaults to false.

Result: `{ mode, position, rotation, yaw, pitch, speed_level, speed, follow_distance, anchor, target }`.

No error codes beyond the universal set.

```json
spectator.set {"position": [0.0, 5.0, 10.0], "yaw": 0.5, "mode": "follow", "target": [0.0, 1.0, 0.0]}
→ {
  "anchor": [
    -2.8765532970428467,
    5.0,
    4.734504699707031
  ],
  "follow_distance": 6.0,
  "mode": "follow",
  "pitch": 0.0,
  "position": [
    0.0,
    5.0,
    10.0
  ],
  "rotation": [
    0.0,
    0.24740396440029144,
    0.0,
    0.9689124226570129
  ],
  "speed": 8.0,
  "speed_level": 0,
  "target": [
    0.0,
    1.0,
    0.0
  ],
  "yaw": 0.5
}
```

<!-- capture:spectator.set -->

### `spectator.step`

Advance the spectator camera by frames of dt seconds under one constant input; speed_steps and zoom apply on the first frame only.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `frames` | number | no | Frames to integrate, 0..=100000. Default 1. |
| `dt` | number | no | Seconds per frame. Default 1/60; frames over 0.25 s are clamped. |
| `movement` | array | no | [right, up, forward], each -1..1. |
| `look` | array | no | [yaw delta, pitch delta] per frame, radians. A third value is ignored. Length 1 or 4 is refused. |
| `speed_steps` | number | no | Speed level change. |
| `zoom` | number | no | Follow orbit distance change in meters. |

`frames` is from 0 to 100000.

Movement defaults to `[0, 0, 0]`; look defaults to no turn; speed_steps and zoom default to 0. `look` is `[yaw, pitch]` or `[yaw, pitch, ignored]`, finite numbers. Speed changes and zoom apply only on the first frame of the call.

Result: `{ frames, mode, position, rotation, yaw, pitch, speed_level, speed, follow_distance, anchor, target }`.

No error codes beyond the universal set.

```json
spectator.step {"frames": 60, "dt": 0.016, "movement": [0.0, 0.0, 1.0]}
→ {
  "anchor": [
    -0.023673273622989655,
    1.032918930053711,
    0.038963720202445984
  ],
  "follow_distance": 6.0,
  "frames": 60,
  "mode": "follow",
  "pitch": 0.0,
  "position": [
    2.8528800010681152,
    1.032918930053711,
    5.304459095001221
  ],
  "rotation": [
    0.0,
    0.24740396440029144,
    0.0,
    0.9689124226570129
  ],
  "speed": 8.0,
  "speed_level": 0,
  "target": [
    0.0,
    1.0,
    0.0
  ],
  "yaw": 0.5
}
```

<!-- capture:spectator.step -->

## Workflow: record a few steps and scrub back

1. In an app with `DemoWorldPlugin`, call `demo.record_start` with the level, seed, tick rate and keyframe interval. `demo_not_installed` means the plugin is absent.
2. Let the game run (or `play.step`), then `demo.record_stop` with a project-relative `.regiademo` path to save the recording.
3. `demo.inspect` on the saved path for the header, the step count and the keyframe index. `demo.seek_plan` names the keyframe a seek would restore without running the demo.
4. `demo.play` with the saved path, then `demo.status`. Requests apply at the end of the frame, so read status after the play call has been applied.
5. `demo.seek` to a tick, `demo.pause`, and `spectator.set` / `spectator.step` to move the spectator camera while the replay is paused. `spectator.bind` writes that pose onto a camera entity.

<!-- /depth-2:method-reference -->
