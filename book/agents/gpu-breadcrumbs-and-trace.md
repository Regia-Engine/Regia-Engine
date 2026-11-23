# GPU breadcrumbs and trace markers (agents)

Two diagnostics that answer "where was the GPU when the device was lost?" and "what happened when?".
Design: `docs/specs/54-gpu-breadcrumbs-and-trace-markers.md`, ADR-1671, ADR-2307. `AgentPlugin` installs every
method below except `render.fault_inject` in each app with the agent (`regia_agent::install_builtin_extensions`); they
live on the world's `AgentExtension`. A test host adds `render.fault_inject` with
`regia_agent::breadcrumbs::install_breadcrumb_methods` in a build with the `test-hooks` feature.

| Method | Permission | Params | Result |
|---|---|---|---|
| `render.breadcrumbs` | read | `frames?` (1-64, default 4), `passes?` (default true), `source?` (`live` or `last_bundle`) | `{ source, level_number, report }` |
| `telemetry.mark` | play | `name` (1-64 characters), `data?` (object, at most 1 KiB) | `{ t_ns, frame, recorded }` |
| `telemetry.range_open` | play | `name`, `lane?` (`main`, `render`, `worker:<n>`) | `{ id, depth, recorded }` |
| `telemetry.range_close` | play | `id` | `{ duration_ns, forced_close: [id] }` |
| `telemetry.trace` | play | `action` (`start`, `stop`, `save_window`, `status`), `streams?`, `window_seconds?`, `file?` | `{ state, file, bytes, events, dropped, ranges_open, degraded }` |
| `telemetry.screenshot_mark` | play | `name?` (1-64 characters), `size?` (`[width, height]`, 1-4096 each) | `{ file, frame, t_ns, recorded, width, height, mime, png_base64 }` |
| `render.fault_inject` | play, `test-hooks` builds only | `kind`, `submission?`, `at_frame?`, `mode?`, `pass?` | `{ armed, fires_on_frame, pass? }` |

`telemetry.screenshot_mark` answers when the renderer has captured the window: the PNG is written next to the
file being written, as `<trace>.shots/<n>.png` (numbered from 1), and the trace gets a screenshot mark with the
frame and that relative path. The answer carries the image too (`png_base64`). It needs a file being written
(`trace_not_writing` otherwise) and a renderer (`no_renderer`).

## Reading an evidence level

`render.breadcrumbs` returns `{ source, level_number, report }`. `report` is a `BreadcrumbReport`
(`format_version` 1). `level_number` is that report's evidence level as an integer. `level`, `frames`
and `format_version` sit on `report`, not on the result. It states the best evidence it has and
never implies more:

| `level` | Number | What it can say |
|---|---|---|
| `timestamped` | 3 | The last pass whose closing GPU timestamp resolved; the passes after it are the suspects |
| `fenced` | 2 | Submissions up to `counters.completed` finished; the later ones were in flight, and `suspects` lists their passes in GPU order |
| `recorded` | 1 | CPU pass order only; the suspects are the whole newest frame |
| `none` | 0 | The writer is off or nothing was recorded: frame index and adapter only |

`counters` has `submitted`, `completed` (the highest index such that it and every earlier submission completed)
and `acknowledged_frame`. `submitted - completed` is the in-flight depth, normally 1 to 3. `frames` is newest
first; each has the pass names in the order the GPU received them. Names are a contract: renaming a pass is a
release-note line.

Every GPU pass of the frame opens a breadcrumb named after its profiling span: `shadow_cascades`,
`depth_prepass`, `cluster_lights`, `ddgi`, `ssao`, `contact_shadows`, `opaque`, `ssgi`, `ssr`, `sssr`,
`atmosphere`, `clouds`, `fog`, `taau` or `tsr`, `lens`, `translucent_layers`, `post`, `ui` (the UI overlay layer) and
the display stage, among others. A pass that did not run that frame is not listed.

**Timestamp backing (level 3).** `BreadcrumbConfig { timestamps: TimestampMode::WhenSupported, .. }` (default off)
turns on the frame's GPU timing queries on a device with `timestamp_query`; each frame whose end stamp resolves
(two frames late) is recorded as fully finished. A loss report then says `timestamped`: the suspects are the
passes after the last resolved frame that had not completed. Without timestamp queries (common on Android) the
report stays `fenced`.

**Crash reports.** When the device is lost, the engine also writes a `crash-<time>.txt` crash report (headline
`device_lost: <reason>: <message>`) with the same `gpu breadcrumbs:` block, and every panic report carries the
block too. The next launch's crash reporter (`regia_crash`) uploads it like any crash report, so a Steam crash
report names the passes the GPU was most likely inside.

`play.wait_until` can poll `render.breadcrumbs` counters; `play.expect` on `report.level` checks the machine class.
`source: "last_bundle"` parses `gpu_breadcrumbs.json` from the newest crash bundle (`no_bundle` when there is
none), so an agent can read a report after the game exited with code 3.

## Mark, run, record, read back

1. `telemetry.trace { "action": "start", "file": "boss" }` starts `boss.regiatrace` in the profiling folder.
   When the rolling window is on, the window is written first, so the file starts before the call.
2. `telemetry.mark { "name": "boss door opened" }`, and `telemetry.range_open` / `telemetry.range_close`
   around a stretch of work.
3. `play.step { "frames": 30 }`.
4. `telemetry.trace { "action": "stop" }` returns the summary; open ranges are closed with `forced_close`.
5. `regia trace export boss.regiatrace --format trace-event --out boss.json` writes the public Trace Event
   Format for any generic viewer; `--format jsonl` writes one JSON object per event for tools.

Trace files are written on a writer thread with a bounded queue: a frame that records marks or passes never
waits for the disk. When the disk falls far behind, the recording stops at its last complete chunk and the
summary says `degraded`. While a file is being written, the `frames` stream records each frame's begin and end
and the `passes` stream the breadcrumb passes on the `gpu` lane.

`telemetry.trace { "action": "save_window" }` writes the rolling window (the newest seconds in memory) to a
separate file and leaves a running file alone. `status` never changes state. Marks also emit a `regia::mark`
tracing event, so a host with the JSON log layer sees them without a trace file.

Limits: at most 100 marks per second per name (the rest are dropped and counted in `dropped`), 32 ranges deep
per lane, 64 characters per name. Closing a range that is not the innermost on its lane closes the inner ones
first and returns their ids in `forced_close`.

## Recording from launch

`TracePlugin` (in `regia::DefaultPlugins`, ADR-9070) starts a recording at the first frame when the launch asks
for one, so a trace covers loading and the first frames before an agent connects:

- `game --record-trace` records `trace-<unix_ms>.regiatrace`; `game --record-trace=boot` records `boot.regiatrace`
  (a bare name; the argument after `--record-trace` is never taken as the name).
- Without the flag, `REGIA_RECORD_TRACE=1` (or `on`, `true`, `yes`) records under the default name and any other
  value except `0`, `off`, `false`, `no` and empty is the file name.
- On Android there are no launch arguments: `adb shell run-as <package> sh -c 'echo boot > files/record-trace'`
  records `boot.regiatrace` on the next launch (an empty file records under the default name).

Trace files land in the profiling folder: `profiling/` under the working directory on desktop, and
`<external files dir>/profiling` on Android (`/sdcard/Android/data/<package>/files/profiling`), so
`adb pull` reaches them without `run-as`. `telemetry.trace { "action": "status" }` shows the running file; the clean
exit stops a recording that is still running, so the file ends with its index chunk. A host that wants other
settings adds its own `TracePlugin { config, args }` before `DefaultPlugins`, or calls
`regia_telemetry::trace::configure`.

## Render graph nodes

Every live node of a `RenderGraph` run (`RenderGraph::execute`) and every pass a game registers in the frame
graph opens one breadcrumb scope named by its id, in the order the graph runs them (ADR-9070); a culled node opens
none. They show in `render.breadcrumbs` frame pass lists and in a recording's `passes` stream on the `gpu` lane,
next to the renderer's own passes. A host that runs a graph on its own encoder and submits it itself calls
`regia_render::breadcrumbs::note_untracked_submit(&queue)` after `queue.submit`, so the scopes become that
submission's record.

**Check:** `telemetry.trace { "action": "start", "file": "graph" }`, `play.step { "frames": 2 }`,
`telemetry.trace { "action": "stop" }`, then `regia trace export graph.regiatrace --format jsonl`: each frame's
`pass_open` / `pass_close` events list the graph's node ids in order.

## Provoking a loss in tests

`render.fault_inject` exists only when `regia_agent` is built with the `test-hooks` feature; shipping builds
never contain it. It arms a fault that fires on the first submit of `at_frame` or later:

- `device_lost` (`mode: "scripted"`, default): raises a device loss through the real path. The render plugin
  stops drawing, runs the device-lost hooks, writes the bundle and exits with code 3.
- `device_lost` with `mode: "destroy"`: calls `Device::destroy()` on the render device, so wgpu's own loss
  callback runs (on a software adapter such as lavapipe this proves the plumbing, not driver behavior).
- `stall_at_submission { submission }`: submissions from that index on stop reporting completion, then the
  device is lost, so the report is `fenced` with those submissions' passes as suspects.
- `oom`: an out-of-memory pressure signal.
- `device_lost` with `pass` (scripted only): the device is lost inside that pass. When a frame at or after
  `at_frame` opens it, the submission that carries it never completes, the scripted timestamps resolve every
  pass before it, and the loss fires right after that submit. The report is `timestamped` with that pass as the
  first suspect, and the crash report's `gpu breadcrumbs:` block names it. The pass must have run already
  (`unknown_pass` otherwise; `render.breadcrumbs` lists the names).

A runnable check on lavapipe (from a test host with `test-hooks`):

```json
{ "method": "render.breadcrumbs", "params": { "frames": 1 } }
{ "method": "render.fault_inject", "params": { "kind": "device_lost", "pass": "opaque" } }
{ "method": "play.step", "params": { "frames": 2 } }
{ "method": "render.breadcrumbs", "params": { "source": "last_bundle" } }
```

The last call answers `level: "timestamped"` and `suspects[0].name: "opaque"`; the process exits with code 3.

## Errors

`F0002` (bad params) or `F0003` (not found) with `data.reason`:

| Reason | Code | Meaning |
|---|---|---|
| `breadcrumbs_off` | F0003 | `render.breadcrumbs` while the writer is disabled |
| `no_bundle` | F0003 | `source: "last_bundle"` with no crash bundle, or one without the entry |
| `unknown_range` | F0003 | `telemetry.range_close` with an id that is not open |
| `name_too_long` | F0002 | A name over 64 characters |
| `data_too_large` | F0002 | Mark `data` over 1 KiB |
| `invalid_lane` | F0002 | A lane that is not `main`, `render` or `worker:<n>` (`gpu` is engine-written only) |
| `trace_busy` | F0002 | `start` while a file is being written |
| `trace_not_writing` | F0002 | `stop` with no file, `save_window` with no window, or `telemetry.screenshot_mark` with no file being written |
| `no_renderer` | F0003 | `telemetry.screenshot_mark` in an app without a renderer |
| `screenshot_failed` | internal | The capture or its PNG encoding failed |
| `unknown_pass` | F0003 | `render.fault_inject` `pass` that has not run yet |
| `bad_file_name` | F0002 | A file name with separators, `..`, a leading dot or a NUL |
| `trace_io` | internal | The file could not be created or written |
| `invalid_value`, `unknown_field` | F0002 | Wrong type or range, or an unknown parameter |
| `test_hooks_off` | F0003 | `render.fault_inject` `destroy` with no render device |

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `render.breadcrumbs`

GPU breadcrumbs: the evidence level, submission counters, suspect passes and the newest frames' pass lists.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `frames` | integer | no | Newest frames to return: default 4; integer 1..=64. The report may have fewer available frames. |
| `passes` | boolean | no | include each frame's pass names (default true) |
| `source` | string | no | `live` (default) reads the ring; `last_bundle` reads the newest crash bundle |

Result: `{ source, level_number, report: { format_version, level, adapter, lost, counters, suspects, frames, truncated } }`.

Errors besides the universal codes: `F0003`. `data.reason`: `breadcrumbs_off`, `invalid_value`, `no_bundle`, `unknown_field`, `internal` (report serialization, `F0001`).

```json
render.breadcrumbs {"frames": 2, "source": "live"}
→ {
  "report": {
    "adapter": {
      "backend": "",
      "device": 0,
      "device_type": "",
      "driver": "",
      "driver_info": "",
      "name": "",
      "vendor": 0
    },
    "counters": {
      "acknowledged_frame": 0,
      "completed": 0,
      "submitted": 0
    },
    "format_version": 1,
    "frames": [],
    "level": "none",
    "lost": null,
    "suspects": [],
    "truncated": false
  },
  "source": "live",
  "level_number": 0
}
```

`level_number` is the numeric evidence level; the handler returns it beside the report.

<!-- capture:render.breadcrumbs -->

### `render.fault_inject`

Test builds only: provoke a device loss, a submission stall then loss, or an out-of-memory signal.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `kind` | string | yes | `device_lost`, `stall_at_submission` or `oom` |
| `submission` | integer | no | for `stall_at_submission`: the first submission that stops completing |
| `at_frame` | integer | no | fire on the first submit of this frame or later (default: the next submit) |
| `mode` | string | no | `scripted` (default) or `destroy` (Device::destroy on the render device) |
| `pass` | string | no | for scripted `device_lost`: lose the device inside this pass (a name `render.breadcrumbs` has listed) |

Result: `{ armed: true, fires_on_frame, pass? }`.

Errors besides the universal codes: `F0002`, `F0003`. `data.reason`: `invalid_value`, `unknown_field`, `unknown_pass`, `test_hooks_off`.

Registered only in builds with the `test-hooks` feature. A normal editor or game answers that the method is unknown.

```json,ignore
render.fault_inject {"kind": "oom", "mode": "scripted"}
→ { "message": "unknown method render.fault_inject", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:render.fault_inject -->

### `telemetry.mark`

Add a named instant to the trace recorder (and a regia::mark log event).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `name` | string | yes | 1-64 characters |
| `data` | object | no | extra JSON, at most 1 KiB |

Omitted data adds no extra payload.

Result: `{ t_ns, frame, recorded }`.

Errors besides the universal codes: `data.reason`: `data_too_large`, `invalid_value`, `name_too_long`, `unknown_field`.

```json
telemetry.mark {"name": "boss door opened"}
→ { "frame": 0, "recorded": true, "t_ns": 11933415 }
```

<!-- capture:telemetry.mark -->

### `telemetry.range_close`

Close a range; inner ranges still open close first and are reported as forced.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `id` | integer | yes | the id telemetry.range_open returned |

Result: `{ duration_ns, forced_close: [id] }`.

Errors besides the universal codes: `F0003`. `data.reason`: `invalid_value`, `unknown_field`, `unknown_range`.

```json
telemetry.range_close {"id": 1}
→ { "duration_ns": 11145663, "forced_close": [] }
```

<!-- capture:telemetry.range_close -->

### `telemetry.range_open`

Open a named range on a CPU lane of the trace recorder; close it with telemetry.range_close.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `name` | string | yes | 1-64 characters |
| `lane` | string | no | `main` (default), `render` or `worker:<n>` |

Result: `{ id, depth, recorded }`.

Errors besides the universal codes: `data.reason`: `invalid_lane`, `invalid_value`, `name_too_long`, `unknown_field`.

```json
telemetry.range_open {"name": "stress", "lane": "main"}
→ { "depth": 0, "id": 1, "recorded": true }
```

<!-- capture:telemetry.range_open -->

### `telemetry.screenshot_mark`

While a trace file is being written: capture the window, write the PNG next to the trace and record a screenshot mark that references it.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `name` | string | no | 1-64 characters |
| `size` | array | no | [width, height] to scale the shot to (1-4096 each; default the window size) |

Omitted name leaves the mark unnamed. Omitted size keeps the window size; supplied dimensions are integers in 1..=4096.

Result: `{ file, frame, t_ns, recorded, width, height, mime, png_base64 }`.

Errors besides the universal codes: `F0003`. `data.reason`: `invalid_value`, `name_too_long`, `no_renderer`, `screenshot_failed`, `trace_io`, `trace_not_writing`, `unknown_field`.

```json
telemetry.screenshot_mark {"name": "door open", "size": [640, 360]}
→ { "file": "boss.regiatrace.shots/1.png", "frame": 2, "height": 360, "mime": "image/png", "png_base64": ..., "recorded": true, "t_ns": 5187049456, "width": 640, ... }
```

Elided: `png_base64` (png 78640 chars).

<!-- capture:telemetry.screenshot_mark -->

### `telemetry.trace`

Run the trace recorder: start or stop a .regiatrace file, save the rolling window, or read the status.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `action` | string | yes | `start`, `stop`, `save_window` or `status` |
| `streams` | array | no | for start: any of `frames`, `passes`, `counters`, `logs` (`marks` is always on); Omitted keeps the current stream set, initially marks, frames, passes |
| `window_seconds` | number | no | For start: 0..=600 seconds; 0 turns the window off. Omitted keeps the current window size. |
| `file` | string | no | For start and save_window: a bare file name in the profiling folder; omitted generates trace-&lt;unix_ms&gt;.regiatrace. |

Result: `{ state, file, bytes, events, dropped, ranges_open, degraded }`. File is null when no file is associated with the recorder.

Errors besides the universal codes: `data.reason`: `bad_file_name`, `invalid_value`, `trace_busy`, `trace_io`, `trace_not_writing`, `unknown_field`.

```json
telemetry.trace {"action": "start", "file": "boss"}
→ { "bytes": 46, "dropped": 0, "events": 0, "file": "profiling/boss.regiatrace", "ranges_open": 0, "state": "writing", "degraded": false }
```

`degraded` is true when a write error closed the trace file early.

<!-- capture:telemetry.trace -->

## Workflow: mark a moment and read it back

1. `telemetry.trace` with `action` `status` so you know whether a file is already open (`trace_busy` if you start a second one).
2. `telemetry.trace` `start` with a bare `file` name. `bad_file_name` refuses a path, not a name.
3. `telemetry.mark` with a short `name` at the moment you care about. Open a range with `telemetry.range_open`, do the work, then `telemetry.range_close` with the `id` you were given.
4. `render.breadcrumbs` with `source` `live` and a small `frames`. `breadcrumbs_off` means the evidence level is off; `no_bundle` means you asked for `last_bundle` and there is no crash bundle.
5. While writing, optionally call `telemetry.screenshot_mark` and wait for its answer, then `telemetry.trace` with action stop. A screenshot mark after stop returns `trace_not_writing`.
6. Leave `render.fault_inject` to a `test-hooks` build. It is not registered on a normal editor or game.

<!-- /depth-2:method-reference -->
