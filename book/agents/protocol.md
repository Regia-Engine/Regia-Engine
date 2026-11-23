# Driving a running Regia Engine app (agent protocol 0.2)

Every dev build of a Regia Engine game (feature `agent`, on by default) runs a small server that lets an AI
agent — or a script — inspect and drive it while it runs. This page is written for agents.

## Connecting

The app writes `<project>/.regia/agent/endpoint.json` when it starts:

```json
{ "url": "ws://127.0.0.1:41234", "pid": 4242, "protocol": "0.2", "token_file": ".../token", "app": "hello_window" }
```

- Connect with a WebSocket to `url` (it must be `ws://127.0.0.1:<port>`), sending
  `Authorization: Bearer <contents of the token file next to endpoint.json>` in the
  handshake. The client always reads `<dir>/token`; the `token_file` field is informational and never followed.
  `endpoint.json` is mode 0600 on Unix; on Windows both files get a DACL that names only the
  current account, `SYSTEM` and `Administrators` (no inherited entries; ADR-5489). Connections without the token, or with a browser `Origin` header, are refused.
- Send JSON-RPC 2.0 requests as text messages; each gets one response with the same `id`.
  A response of 64 KiB or more (a screenshot) arrives as one text message split into 64 KiB
  frames (RFC 6455 § 5.4 continuation frames, ADR-8848); WebSocket libraries reassemble it.
- Easiest: `regia agent call <method> '<params json>' --project <dir>`, or the MCP server
  `regia mcp --project <dir>` (one MCP tool per protocol method). Attached to an editor
  (or with `--headless`), the tool list also holds the editor's own methods that have a tool
  name, read from the editor's live `agent.schema` each time the client lists tools:
  `save_level`, `undo`, `redo`, `select`, `focus`, `import`, `create_asset`, `create_script`,
  `play`, `stop` and the rest. Where an editor method shares a tool name with a runtime method
  (`pause`, `step`, `set_property`, `list_assets`), the tool calls the runtime method; call
  the editor one by its method name (`editor.pause`) with `regia agent call` (ADR-5628).
- The file disappears when the app exits. If it is missing, start the app (`cargo run -p hello_window`).
- Tell the app who you are: `agent.hello` with `client` and `model` — both land in the audit log and
  on every provenance record of your edits.

Calls take effect at the next frame boundary (usually within 16 ms). If the window is on a hidden
workspace or minimized, the app keeps running at 10 frames per second, so calls still work.

## The surface

The schema is the source of truth: `agent.schema` returns every method with its parameters, result
shape, whether it mutates, and which permission it needs. The MCP tool list is generated from the
same table. Topic pages (also bundled as `agent.guide`): **[scene](scene.md)** —
**[testing](testing.md)** — **[seeing](seeing.md)** — **[assets](assets.md)** —
**[events](events.md)** — **[gameplay](gameplay.md)** — **[framework](framework.md)** —
**[physics](physics.md)** — **[safety](safety.md)** — **[renderer](renderer.md)** —
**[high-end lighting](high-end-lighting.md)**.

| Area | Methods (MCP tool) | Purpose |
|---|---|---|
| Discovery | `agent.hello` (`hello`), `agent.guide` (`guide`), `agent.schema` (`schema`), `agent.changes` (`changes`), `project.info` (`project_info`), `types.list` (`list_types`), `types.describe` (`describe_type`), `resources.list` (`list_resources`), `errors.explain` (`explain_error`) | What exists and what it means: reflected types with JSON Schema + docs, resources, your own change history, long-form error help |
| Scene | `scene.query_entities` (`query_entities`), `scene.get_entity` (`get_entity`), `scene.set_property` (`set_property`), `scene.spawn` (`spawn`), `scene.spawn_primitive` (`spawn_primitive`), `scene.despawn` (`despawn`), `scene.duplicate` (`duplicate`), `scene.reparent` (`reparent`), `scene.add_component` (`add_component`), `scene.remove_component` (`remove_component`), `scene.apply_edits` (`apply_edits`), `scene.serialize` (`serialize_scene`) | Find, inspect and edit entities through reflection |
| Assets | `assets.list` (`list_assets`), `assets.describe` (`describe_asset`), `assets.load` (`load_asset`), `assets.reload` (`reload_asset`), `assets.skin_check` (`skin_check`), `assets.status` (`asset_status`), `assets.load_progress` (`load_progress`), `assets.watch_status` (`asset_watch_status`), `assets.texture_memory` (`texture_memory`) | The running AssetServer's manifest and load state; load-set residency; live reload status; texture memory against the budget |
| Seeing | `seeing.screenshot` (`screenshot`), `seeing.capture_sequence` (`capture_sequence`), `seeing.contact_sheet` (`contact_sheet`), `seeing.compare_images` (`compare_images`), `seeing.reference_trace` (`reference_trace`), `seeing.compare` (`compare`), `seeing.pick_entity` (`pick_entity`), `seeing.memory_report` (`memory_report`) | Screenshots, strips, orbit sheets, SSIM + diff, reference traces and FLIP scoring, GPU memory by category |
| Render | `render.boot_timeline` (`boot_timeline`), `render.pipeline_warm` (`pipeline_warm`), `render.memory_budget` (`memory_budget`), `render.camera_cut` (`render_camera_cut`), `render.out_start` / `render.out_progress` / `render.out_cancel` (`render_out_start` / `render_out_progress` / `render_out_cancel`), `render.out_job` (`render_out_job`), `render.sequence` (`render_sequence`), `camera.set_path` (`camera_set_path`), `camera.raycast` (`camera_raycast`) | Boot timeline, pipeline warm-up per tier and the persistent cache, memory budget and preload admission, camera cuts ([camera cuts](camera-cuts.md)), offline fixed-dt render-out ([render-out](render-out.md)), render jobs from a shot list or sequence ([render jobs](render-jobs.md)), camera paths, shot sequences and screen raycasts ([camera paths](camera-paths.md)) |
| Play & test | `play.pause` / `play.resume` (`pause` / `resume`), `play.step` (`step`), `play.set_time_scale` (`set_time_scale`), `play.inject_input` (`inject_input`), `play.wait_until` (`wait_until`), `play.expect` (`expect`), `play.get_diagnostics` (`get_diagnostics`) | Deterministic play, input injection, predicates and assertions |
| Logs & events | `logs.read` (`read_logs`), `events.list` (`list_events`), `events.read` (`read_events`), `diagnostics.crash_reports` (`crash_reports`), `feedback.capture` (`feedback_capture`), `feedback.list` (`feedback_list`), `telemetry.get_last_summary` (`telemetry_get_last_summary`), `telemetry.live` (`telemetry_live`) | Log ring and tapped engine messages, both with cursors, the local crash reports, feedback bundles, the last telemetry summary and the newest frame's live signals ([telemetry](telemetry.md)) |
| Framework | `scripts.list`, `scripts.get`, `camera.get`, `camera.set_mode`, `settings.get`, `settings.set`, `level.get_look`, `level.set_look`, `sim.counters`, `sim.check_framerate_independence` | Presentation scripts, camera rigs, live settings, DO27 sim-bridge counters |
| Gameplay | `players.list` (`players_list`), `players.get` (`players_get`), `scene.possess` (`possess`), `save.save` (`save_game`), `save.load` (`load_game`), `save.list_slots` (`list_save_slots`) | Who is playing, what they possess, save slots and round trips — see [gameplay](gameplay.md) |
| Terrain | `terrain.scatter` (`terrain_scatter`), `terrain.set_scatter` (`terrain_set_scatter`), `terrain.stroke` (`terrain_stroke`) | Rock / prop scatter counts and toggles — see [terrain scatter](terrain-scatter.md); sculpt and paint brush strokes — see [terrain editing](terrain-editing.md) |
| Renderer | `render.set_contact_shadows` (`render_set_contact_shadows`), `render.set_reflection_occlusion` (`render_set_reflection_occlusion`), `render.set_near_sun_shadow` (`render_set_near_sun_shadow`) | Screen-space contact shadows on/off — see [contact shadows](contact-shadows.md); reflection occlusion on/off — see [reflection occlusion](reflection-occlusion.md); near sun tile edge — see [near sun shadow](near-sun-shadow.md) |

MCP resources (§ 3.8): `regia://project`, `regia://scene/current` (RON), `regia://types/{path}`,
`regia://assets/{id}`, `regia://logs/recent`, `regia://guide/{topic}`,
`regia://protocol/schema.json`. URIs with the scheme from before the rename still resolve, to the
same calls, for one release (see [Renaming to Regia Engine](../user-guide/rename-regia.md)).

Entity ids are numbers; take them from `scene.query_entities`. Component and type names are the short
names `get_entity` lists (`Window`, `Transform`, `Name`). Values use the JSON shape `get_entity`
shows — note that maths types (glam `Vec3`) serialize as **arrays** (`[x, y, z]`), so `set_property`
on `translation` takes `[1.0, 2.0, 3.0]`, while *paths* use reflection field names
(`"translation.y"`).

## Discovery calls

Each line is `regia agent call <method> '<params json>' --project <dir>`; the params are the JSON-RPC `params`.

```text
agent.hello {"client": "my-script", "model": "my-model"}
agent.guide {}                                   # list topics
agent.guide {"topic": "scene"}                   # one topic's text
agent.schema {}                                  # every method, params, result, permission
project.info {}                                  # engine, app, project name and id, subsystems
types.list {"filter": "transform", "component": true}   # reflected components whose name contains "transform"
types.list {"resource": true}                    # reflected resources only
types.describe {"type": "Transform"}             # JSON Schema, docs and an example value
resources.list {}                                # resources, reflected or not
errors.explain {"code": "F0012"}                 # long-form meaning and fix
```

`types.describe` schemas describe the serialized value. Data-carrying enum variants use one tag property:
`{"Named":"a"}` or `{"Rect":{"rect":[0,0,1,1]}}`; unit variants are strings such as `"Earth"`. Tuple payloads
and tuple structs use ordered arrays; newtype variants and one-field tuple structs without serialization
skips use their inner value. Fields skipped during tuple serialization are omitted from the array. `Vec3` value schemas use three numeric array elements, as in the
wire values above; reflection property paths still use field names such as `translation.y`.

`types.describe` accepts a full type path or a unique short path. An unknown path is `F0003`
(`no reflected type …`). A short path shared by two registered types is `F0002` with `data.reason`
`ambiguous_short_type_path`; the message says `ambiguous short type path` and does not say the type
is not registered. `types.list` shows the full path that selects one type.

## Errors

Errors are JSON-RPC errors with a stable Regia Engine code in `error.data.code` and a fix in
`error.data.hint`; `errors.explain` returns the long form.

| Code | JSON-RPC | Meaning |
|---|---|---|
| F0001 | -32603 | Internal error — read `logs.read` with `level: "warn"` |
| F0002 | -32602 | Missing or wrong parameters. A present optional parameter of the wrong JSON type (for example `"count": "5"` where an integer is wanted) names itself: `error.data.param` is the parameter and `error.data.expected` the type (`a string`, `a boolean`, `a non-negative integer`, `an integer`, `a number`, `an array`, `an object`). `null` still means "not given". |
| F0003 | -32001 | Entity, component, type or field not found |
| F0004 / F0005 | -32600 / -32700 | Not a text message, `jsonrpc` is not `"2.0"`, or the id is not a string, number or null / not valid JSON |
| F0006 | -32002 | No answer in time (pass `params.timeout_ms` for long operations; at most 24 hours, longer values are capped). Also answered when the app did not take the request off its queue within that time (a paused app). The request is then not run when the app drains it later (the audit log records it as `expired`), so retrying is safe |
| F0007 | -32003 | More mutating calls in a second than the project allows (default 50) |
| F0008 | -32601 | Unknown method (the message lists the available ones) |
| F0009 | -32600 | A `play.step` is already running |
| F0010 / F0011 | -32004 | Screenshot target or renderer not available |
| F0012 | -32005 | Permission denied — see [safety](safety.md) |
| F0013 | -32004 | The engine feature the method needs is not present (the hint names it) |
| F0014 | -32602 | A `wait_until`/`expect` condition is malformed |
| F0015 | -32004 | A multi-frame capture produced no frames |
| F0016 | -32001 | The asset server is not running or the asset is unknown |
| F0017 | -32600 | A `play.hitch_report` is already running |
| F0018 | -32600 | A render-out is running (a second start, or a `play.*` clock control) |
| F0019 | -32004 | The movie format needs `ffmpeg` and none is on `PATH` |
| `too_many_pending` | -32600 | A deferred method's pending list is full (64 `play.wait_until`, 8 multi-frame captures, 2 running `seeing.reference_trace` jobs) |
| `request_too_large` | -32600 | The request would take more than 128 MiB once parsed (32 bytes per JSON value plus every string's bytes); refused before it is built, with a null id |

The UI methods also answer lowercase codes (`unknown_entity`, `not_a_list`, `bad_ease`, ...); each is listed
on its method's page and in `agent.schema`, and `errors.explain` knows every one. A mistyped method name answers
`F0008` with `data.suggestion`, the closest known name, when one is near.

## Current limits

- Screenshots cover the window, mesh-asset preview and `offscreen` targets (default 256×144; see `seeing.md`); camera-entity targets, view modes,
  overlays and pixel picking need renderer support that this build does not expose (they answer
  `F0013` rather than pretending).
- Importing, creating, editing and deleting asset **files** belongs to the editor/pipeline surface;
  the runtime serves the manifest and load state of the running app.
- No runtime undo, staged review queue or `select`/`frame`/`save_level` yet — those are editor-side.
- Android: the server listens inside the device; reach it with `adb forward tcp:<port> tcp:<port>` and read
  the endpoint with `adb shell run-as <package> cat files/agent/endpoint.json`.

## Limits (ADR-340)

At most 16 connections are open at once and requests queue in a bounded inbox; WebSocket messages over 32 MiB are
refused. A connection has 10 s to finish the WebSocket handshake (a silent socket gives its slot back), and at most 8 of
its notifications (requests without `id`) wait for the engine at once; past that it reads the next frame once the oldest
is handled. The audit log keeps at most 128 bytes of a method name and 256 bytes of the `agent.hello` `client` and
`model` (ADR-1769). The mutating-call rate limit is a sliding one-second window. A sequence holds at most 256 shots and 4096
keys per track; `render.out_start` takes at most 4096 `cuts`; `seeing.compare_images` takes PNGs of 1..=4096 pixels a
side. `replay.load` reads regular files only, at most 512 MiB, under the
allowed roots of `render.out_start` (as do `demo.inspect` and `demo.seek_plan`). Errors show project-relative paths, never the host's
absolute ones.

### Parse budget (ADR-2306)

A message under the 32 MiB cap can still parse into far more memory: `[0,0,0,...]` is two bytes of text per
32-byte JSON value. The server counts what the parse builds (32 bytes per value, the bytes of every string and key,
32 more per object entry) and stops at 128 MiB, before the rest is built. Such a request answers `request_too_large`
(JSON-RPC -32600) with a null id; the connection stays open. The largest array a method contract allows (one
million numbers) takes about 32 MiB. Split anything bigger into several calls.

```text
errors.explain {"code": "request_too_large"}     # the long form and the fix
```

### Round 2 (ADR-345)

`seeing.reference_trace` and `seeing.compare` write files, so they need the `edit_scene` permission, count against
the mutating-call rate limit, and confine `out_dir`, `a`, `b` and `map_png` to the same roots as `render.out_start`
(project root, system temp dir and `output_roots` in `.regia/agent.toml`; no `..`, no symlink out).
`replay.step` advances at most 36 000 ticks per call (`clamped: true` when it cut `ticks`) and returns at most 1000
events (`events_total`, `events_truncated`). `moments.list` pages without building the full match list.

### Method contracts (ADR-1034)

Every method has a contract, published by `agent.schema` next to its params (`agent.schema { method }` returns
one entry) and printed in the [method reference](../reference/agent-methods.md):

- `example`: a typical call's params, valid against the schema.
- `errors`: every `data.code` the method can answer with besides the universal ones (`F0001`, `F0002`,
  `F0004`, `F0007`, `F0012`).
- `limits`: ranges, item counts and byte lengths the dispatcher checks before the handler runs. A range on an
  array param (a position, a polyline) applies to every number in it. A refused call answers `F0002` with the
  limit in `data.limit` (`{ param, kind, min, max }`) and costs no rate budget.
- `cost`: `"light"` or `"heavy"`. Heavy calls (whole-world walks, image encode or decode, multi-frame captures,
  replay loads and steps) also count against `heavy_rate_limit_per_second` in `.regia/agent.toml`
  (default 60, one a frame at 60 fps), read-only or not.

`params` must be a JSON object (or absent). An array or string param with no limit of its own holds at most
65 536 items or 1 MiB (`seeing.compare_images` PNGs may be 32 MiB, RON texts 16 MiB). The protocol-wide caps are in `agent.schema`'s top-level `limits`.

### Permission classes (ADR-3285)

Every method needs one permission class (`agent.schema` names it; `.regia/agent.toml` grants them):
`read`, `edit_scene`, `play`, `assets`, `tools`, `build`, `review`. `read_only = true` allows `read` alone.

**The rule for `read`:** a `read` method takes no output path, writes no file and changes no state another call
can observe. A method that does any of these needs the class of what it does and is `mutating` (rate-limited):

- it takes an output path (`out_dir`, `out`, `output_*`, `dest*`, `write`, `save`) or writes a file at all;
- it has a switch (`enable`, `detach`, `clear`, `log`, ...) or an `action` that starts, stops, attaches or resets
  something;
- its name says it changes something (`set_*`, `start`, `stop`, `reset`, `clear`, ...).

Switches beside the renderer's other toggles and writers of project files take `edit_scene` (their changes are
recorded in `agent.changes`); recorders and file writers that a polling client drives take `play`. Answering with
an image or a computed value is still `read` (`seeing.capture_sequence`, `xr.capture_eye`), and so is reading a log. The `security_wave2`
test of `regia_agent` runs this rule over the agent schema, so a new method that breaks it fails the build.
Editor methods are not in that schema. `regia_editor` checks the ones that change a viewport registry, the
editor selection or a panel another call reads (`state_changing_editor_methods_are_not_read`).

The name check has two named exceptions, each for one word of one method whose handler borrows the world
immutably: `spawn` in `vfx.spawn_probe` (it simulates a scratch instance, ADR-4760) and `record` in
`sequence.record_state` (the take recorder whose state it reads, ADR-5957). Every other check (output paths,
switches, writes, `mutating`) still runs on both, and any other method with those words is still refused.

Methods moved out of `read` by this rule:

| Method | Was | Now | Why |
|---|---|---|---|
| `audio.loopgen_render` | `read` | `play`, mutating | `wav: true` writes a WAV into `out_dir` |
| `profiler.set_capture` | `read` | `edit_scene`, mutating | turns call-tree capture on or off |
| `project.describe` | `read` | `edit_scene`, mutating | `write: true` writes `project-map.md` and `.json` (`out_dir`) |
| `render.exposure_log` | `read` | `play`, mutating | `action` starts and stops a log |
| `pacing.trace` | `read` | `play`, mutating | `log` turns per-frame logging on or off; `clear` forgets rows |
| `procgen.set_debug` | `read` | `edit_scene`, mutating | turns the debug overlays on or off |
| `ai.bt_live` | `read` | `play`, mutating | the first call attaches a recorder; `detach` removes it |
| `voice_chat.stats` | `read` | `play`, mutating | `reset` zeroes the session counters |
| `viewer.render_still` | `read` | `play`, mutating | optional `path` writes a PNG (ADR-4760) |
| `looks.render_board` | `read` | `play`, mutating | optional `path` writes a PNG (ADR-4760) |
| `editor.viewport_instance_set` | `read` | `edit_scene`, mutating | writes the instance camera, layout and show flags |
| `editor.viewport_link` | `read` | `edit_scene`, mutating | joins or leaves a camera link group |
| `editor.viewport_instance_remove` | `read` | `edit_scene`, mutating | drops the viewport registry entry |
| `editor.tests_panel_open` | `read` | `edit_scene`, mutating | replaces the editor selection |
| `editor.agent_changes_select` | `read` | `edit_scene`, mutating | sets which change the panel shows |
| `editor.tests_panel_run` | `play`, not mutating | `play`, mutating | queues a play-in-editor run |

## JSON examples in the guide

`cargo xtask agent-examples` (also run by `book-check` and `cargo xtask preflight`) reads every `json` fence in
`book/agents/*.md`, `book/user-guide/agent-protocol.md` and the bundled `agent.guide` topics, so an example an
agent copies is a call that works. A statement in a fence is a JSON-RPC request (`{ "method": .., "params": .. }`),
`method.name { params }`, a bare params object (its method is the one named in backticks between the previous
heading and the fence whose params cover most of the object's keys, the nearest mention on a tie), or a response
after `→`. A request must name a method of the current schema, declare every param, give every required param and
match each declared kind (a number where the param's description says a lone number works, `true` where it names
`true` as a value); a response's top-level keys must be named in the method's `result`. `...` or `…` stands for
elided content (as an entry, a value or an element, also quoted: `"...": "..."`), and `//` comments and trailing
commas are accepted. A fence tagged `json,ignore`, or preceded by `<!-- not-a-request -->`, is
skipped and counted; so are `editor.*` calls, which are host handlers outside the runtime schema. Findings that
cannot be fixed (the page describes behavior the code lacks) go in `docs/ci/agent-examples.toml` with a reason:
a new finding fails, and so does a row with no finding.

## Stability

The surface above is ratcheted: `cargo xtask agent-snapshot` renders every method's name,
permission class, MCP tool name, `mutating` flag, cost class, dispatcher-enforced limits, params
schema (each param's kind, required flag and structured JSON Schema shape, when it has one),
result schema and documented error codes — plus the built-in extension methods and the universal
error codes — into the checked-in `docs/api-snapshots/agent-protocol.txt` and fails when the
tables in `regia_agent` differ from it. It reads the same public tables `agent-coverage` does, so
it runs in seconds and needs no renderer; it is a step of `cargo xtask preflight` and of CI.

Each diff is classified, and the classification is what a client built against the old surface feels:

- **Breaking**: a method or param removed, a param type or shape or the result schema changed, a
  new required param, a permission, MCP tool, `mutating` or cost class changed, a limit tightened,
  an error code removed or renamed.
- **Additive**: anything else — a new method, a new optional param, a new error code, a limit
  relaxed or removed.

A breaking change is accepted only when a release fragment this branch touches (`docs/releases/*.md`)
names the method under `## Upgrade`; an additive change needs a `## New` line naming it (the match
is token-bounded: a line naming `assets.load_progress` does not document `assets.load`). The check
fails until the snapshot is refreshed, and `--update` itself refuses to write a change no touched
fragment documents — a lane that changed the agent schema always ends with two pieces of
bookkeeping: the fragment line and `cargo xtask agent-snapshot --update` (commit the refreshed file
with the code). After merging several schema-changing lanes, an integration branch resolves the
snapshot mechanically the same way (`preflight` forwards its `--base` to the ratchet, so the
fragment diff uses the same merge-base as `--changed`). The failure message of
`cargo xtask agent-snapshot --check` names the method and the exact fix. The Rust API of the engine crates
is held the same way by `cargo xtask api-snapshot` (`docs/api-snapshots/README.md`).
