# Profiler (agents)

The frame profiler records how long each `tracing` span takes per frame (fed by the logging layer; `DefaultPlugins`
installs it). With call-tree capture on, it also records which span ran inside which, and builds a flame graph: the
per-frame mean time of every span position over a window of frames. The main view's GPU pass times come from the
renderer when the device has timestamp queries and GPU timing is on. Design:
`docs/specs/83-prefab-profiler-volumes.md` § 4 (ADR-2323, ADR-9740). With the timeline on, it also records each
span's thread, start, end and nesting depth: [the per-thread timeline](#the-per-thread-timeline) (ADR-7325). The editor shows the same data in the Profiler
window (View → Profiler), writes captures with its Capture button, and reads other processes over this protocol
(see [The editor's Profiler window](#the-editors-profiler-window)).

## Methods

| Method | Permission | What it does |
|---|---|---|
| `profiler.set_capture { flame?, timeline? }` | edit_scene | Turns call-tree capture (`flame`) and the per-thread timeline (`timeline`) on (`true`) or off; at least one is required. Both off by default: each costs one record per span exit while on (at most 65,536 call-tree edges and 8,192 timeline spans per frame). |
| `profiler.frame { frames?, depth?, max_nodes?, since_frame?, entities? }` | read | The CPU call tree over the newest `frames` (1–300, default 60), cut at `depth` (1–32, default 8) and `max_nodes` (1–5000, default 2000), the ten costliest spans, the GPU passes, and the ECS system counts (`systems` interned, `systems_over_cap` past the 2,048-slot cap). `since_frame` (a frame number) adds `frame_times`, every retained frame from that number on, so a poller sees every frame; `entities` (1–200) adds the ECS counts with that many component types ranked. |
| `profiler.capture { path, frames?, depth?, max_nodes?, timeline? }` | edit scene | Writes the same data as a capture file (below) to `path`, relative to the asset server's project root and ending in `.rprof.json`; with `timeline` (default: when the profiler's timeline is on) the window's per-thread timeline too, at most 200,000 spans. A symlink that leaves the project is refused before the file is written. |
| `profiler.compare { a, b, top? }` | read | Compares two capture files (project-relative, `.rprof.json`): per span, matched by its path from the root, the inclusive and self ms of `a` and `b` and `b - a`, largest change first (the first `top` rows, 1–5000, default 50); GPU passes likewise. Deterministic: the same files give the same bytes. |
| `profiler.timeline { frame?, frames?, threads?, max_spans? }` | read | The per-thread timeline of one retained frame (`frame`, a number from an earlier answer; default the newest): per thread its spans with start and end in ns from the frame's start and the nesting depth. `frames` (1–8, default 1) lays that many consecutive frames, ending at `frame`, on one [time axis](#the-per-thread-timeline): the spans keep their place on the axis and the `frames` list gives each frame's start offset. `threads` keeps the named threads; `max_spans` (1–100,000, default 5,000) cuts the list. Needs the timeline on. |

Results:

- `profiler.set_capture`: `{ flame, timeline, frames_recorded }`.
- `profiler.frame`: `{ flame_enabled, timeline_enabled, frames_recorded, frames, frame_ms, nodes, truncated, systems,
  systems_over_cap, cpu: [{ name,
  inclusive_ms, self_ms, calls, children }], top: [{ name, ms }], gpu: [{ pass, ms }] }`. `cpu` lists spans most
  inclusive first (ties by name); `truncated` counts the nodes the limits left out; `top` is the flat per-span
  average (it does not need call-tree capture); `gpu` is empty without timestamps (lavapipe, or timing off).
  `systems` counts the ECS systems interned; `systems_over_cap` the distinct names past the 2,048-slot cap —
  those still appear by name in the timeline and the flame, only the per-slot counters stay capped.
  With `since_frame` the answer adds `frame_times: [{ number, ms, spans: [{ name, ms }] }]` (every retained frame
  numbered `since_frame` or later, oldest first: its wall time and five costliest spans) and `newest_frame`. A poller
  passes `0` first, then the last row's `number + 1`; a `newest_frame` behind its cursor means the process restarted
  (start again at 0). With `entities` it adds `entities: { entities, archetypes, components: [{ name, entities }] }`:
  spawned entities, archetypes and the component types held by the most entities (ties by name), counted over the
  archetype table — the same count as the editor's in-process Entities tab.
- `profiler.capture`: `{ path, frames, nodes, truncated, gpu_passes, timeline_spans, timeline_truncated, bytes }`.
- `profiler.timeline`: `{ frame, first_frame, newest_frame, frame_ns, frame_ms, dropped, spans, truncated, frames: [{ number,
  frame_ns, start_ns, span_ns, dropped }], threads: [{ index,
  name, spans: [{ name, start_ns, end_ns, depth }] }] }`. `first_frame` and `newest_frame` bound the frame numbers
  still retained; `dropped` counts the spans past the profiler's per-frame cap; `truncated` the spans `max_spans` cut.
  With `frames` > 1 the span times are offsets on the strip's combined axis (frame k's first span starts at or after
  frame k-1's last end) and each `frames` row carries that frame's `start_ns` on it.
- `profiler.compare`: `{ frame_ms_a, frame_ms_b, frame_ms_delta, total_spans, spans: [{ path, inclusive_a,
  inclusive_b, inclusive_delta, self_a, self_b, self_delta, calls_a, calls_b, presence }], gpu: [{ pass, ms_a, ms_b,
  delta }] }`. `path` is the list of span names from the root; `presence` is `both`, `only_a` (the span went away) or
  `only_b` (it is new). Rows sort by the absolute inclusive change, then the absolute self change, then the path; every
  number is rounded to the microsecond. The diff is `regia_telemetry::profile_compare::compare`, a pure function.

Times are wall clock and differ between runs: compare shapes and orders of magnitude, never exact values.

## Error reasons (`data.reason`)

| Reason | Code | When |
|---|---|---|
| `no_profiler` | F0003 | No frame profiler is installed (an app without `DefaultPlugins` and without `regia::frame_profiler()` as a layer). |
| `bad_flame` | F0002 | `flame` is missing or not a boolean. |
| `bad_frames` | F0002 | `frames` is not an integer in 1–300 (`profiler.frame`, `profiler.capture`) or 1–8 (`profiler.timeline`). |
| `bad_depth` | F0002 | `depth` is not an integer in 1–32. |
| `bad_max_nodes` | F0002 | `max_nodes` is not an integer in 1–5000. |
| `bad_path` | F0002 | `path` is missing, absolute, contains `..`, or does not end in `.rprof.json`. |
| `no_project_root` | F0003 | No asset server, so no project root to write under. |
| `path_outside_project` | F0002 | `path` is lexically allowed, but a symlink resolves outside the project. Nothing is written. |
| `write_failed` | F0001 | The file could not be written. |
| `path_outside_project` | F0002 | The path escapes the project through a symlink or violates the project path rules; `data.path_reason` names the rule. |
| `golden_path_denied` | F0002 | The capture path names a protected golden directory; the shared request path resolver refuses it. |
| `bad_top` | F0002 | `profiler.compare`: `top` is not an integer in 1–5000. |
| `bad_capture` | F0002 / F0003 | `profiler.compare`: a file is not a capture (or a newer `format_version`), or cannot be read. |
| `bad_timeline` | F0002 | `timeline` is not a boolean. |
| `timeline_off` | F0003 | `profiler.timeline` while the timeline is off (or before a frame was recorded). |
| `bad_frame` | F0002 | `profiler.timeline`: `frame` is not an integer, or that frame left the history. |
| `bad_threads` | F0002 | `profiler.timeline`: `threads` is not an array of names. |
| `bad_max_spans` | F0002 | `profiler.timeline`: `max_spans` is not an integer in 1–100,000. |
| `bad_since_frame` | F0002 | `profiler.frame`: `since_frame` is not a non-negative integer. |
| `bad_entities` | F0002 | `profiler.frame`: `entities` is not an integer in 1–200. |

The stable protocol codes are `F0001` (internal write failure), `F0002` (invalid parameters),
and `F0003` (missing profiler or project root). The dispatcher's limit check (`frames` in 1..=300)
rejects an out-of-range frame window with `F0002` before the handler runs, so the answer is the same
with or without an installed profiler and carries the `limit` it broke (the handler's `bad_frames`
reason applies only when `frames` is not a number):

```json
{"jsonrpc":"2.0","id":7,"method":"profiler.frame","params":{"frames":0}}
→ {"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"`frames` must be in 1..=300 (got 0)","data":{"code":"F0002","hint":"agent.schema lists each method's params and limits","limit":{"kind":"range","max":300.0,"min":1.0,"param":"frames"}}}}
```

## The capture file (`profile-capture` v2, `.rprof.json`)

```json
{
  "format_version": 2,
  "frames": 60,
  "frame_ms": 8.1,
  "cpu": [{ "name": "render", "inclusive_ms": 6.0, "self_ms": 1.0, "calls": 1.0, "children": [] }],
  "truncated": 0,
  "gpu": [{ "pass": "shadows", "ms": 0.9 }],
  "folded": "render 1.0\n",
  "timeline": [{ "number": 41, "frame_ns": 8100000, "dropped": 0, "threads": [
    { "index": 0, "name": "main", "spans": [{ "name": "render", "start_ns": 100000, "end_ns": 6100000, "depth": 0 }] }
  ] }],
  "timeline_truncated": 0
}
```

`folded` is the tree as folded stacks (`a;b;c <self ms>` per line) for flame-graph tools. `timeline` (v2, ADR-7325)
is the per-thread timeline of the captured frames, empty when the timeline was off. A file without `format_version`
(v0) or at v1 reads as v2 with no timeline; a newer one is refused; `regia upgrade-project` re-writes v0 and v1 files
at v2. Captures are diagnostics: the engine never loads them. Format page: `docs/formats/profile-capture-v2.md`.

## The per-thread timeline

`profiler.set_capture { timeline: true }` makes every span exit append its thread, name, start, end and depth to a
buffer of its own thread (reused frame to frame, so a warmed-up frame allocates nothing), up to 8,192 spans a frame
over all threads; later exits that frame are counted in `dropped`. The profiler folds the buffers into its 300-frame
history at the end of each frame. Depth counts the recorded spans open around a span on the same thread (bevy's
scheduling spans are not recorded; an ECS system span is, named by its system). Off, a span exit does no timeline
work. A full history at the cap holds about 118 MB, so leave it off when not looking.

```json
{"method":"profiler.set_capture","params":{"timeline":true}}
{"method":"play.step","params":{"frames":2}}
{"method":"profiler.timeline","params":{"threads":["main"],"max_spans":200}}
{"method":"profiler.set_capture","params":{"timeline":false}}
```

### The multi-frame strip

`profiler.timeline { frames: N }` (1–8) answers N consecutive retained frames — the strip ends at
`frame` (or the newest) and reaches back N−1 frames further — laid on **one** time axis: each
frame's spans are shifted past the extents of the frames before it, so frame k's first span
starts at or after frame k-1's last end, and threads of the same name share their rows across
frames. Each `frames` row carries that frame's number and `start_ns` on the axis; the editor's
Timeline tab draws a boundary line and the frame number at each start and names the frame a
hovered bar belongs to (`editor.profiler_timeline { frames }` answers the same layout).

```json
{"method":"profiler.timeline","params":{"frames":4,"max_spans":20000}}
```

ECS systems past the frame profiler's 2,048-slot cap (`MAX_SYSTEMS`) are not lost either: each
keeps its own name in the timeline and the flame records and is timed by name in the frame
table, only the aggregated per-slot counters stay capped — `profiler.frame`'s `systems_over_cap`
counts them.

## Example: where does a frame go

Workflow: enable call-tree capture, step a fixed number of frames, inspect the bounded summary,
save a project-relative capture, then turn capture off.

```json
{"method":"profiler.set_capture","params":{"flame":true}}
{"method":"play.step","params":{"frames":60}}
{"method":"profiler.frame","params":{"frames":60,"depth":4}}
{"method":"profiler.capture","params":{"path":"captures/before.rprof.json"}}
{"method":"profiler.set_capture","params":{"flame":false}}
```

The stock editor installs the same process profiler in its minimal edit app and logging
layer, including `--headless --no-gpu`. CPU flame capture does not require a renderer.

## The editor's Profiler window

The window (View → Profiler) has a **Source** picker, a Capture / Open / Set A / Set B row, six tabs, and six more
(Audio, Physics, UI, Rendering, Monitors, Loads; ADR-8141). Its agent
twins are editor methods (they need the editor; a game has the `profiler.*` methods above):

| Method | Permission | What it does |
|---|---|---|
| `editor.profiler_state { top? }` | read | What the window shows for its source: `{ source: { kind, label, address, process }, status, message, frame_ms, frames, top: [{ name, ms }], cpu, history: { frames, median_ms, hitch_threshold_ms, hitches: [{ index, ms, spans }] }, frozen, entities: { entities, archetypes, components: [{ name, entities }] }, memory: { subsystems, owners, top_gpu, assets_by_type, heap, gpu }, opened, compare: { a, b } }`. `top` 1–200, default 10. |
| `editor.profiler_source { kind, address?, token? }` | edit_scene | Chooses the source; answers like `editor.profiler_state`. |
| `editor.profiler_capture_slot { slot, path? }` | edit_scene | Opens a `.rprof.json` (project-relative or absolute) into `view` (shown instead of the live tree), `a` or `b` (the Compare sides); no `path` clears the slot. Answers `{ view, a, b, diff }`, `diff` as `profiler.compare` when both sides are set. |
| `editor.profiler_timeline { frame?, frames?, zoom?, pan?, width?, max_bars? }` | edit_scene | The Timeline tab for its source, laid out in a view (default the tab's own; `zoom` 1–10,000, `pan` 0–1, `width` in points): `{ status, held, frame, frame_ms, dropped, spans, visible_spans, window_ns, frames: [{ number, frame_ns, start_ns, span_ns, dropped }], threads: [{ name, first_row, rows }], rects, merged_rects, bars: [{ label, thread, row, depth, x, width, start_ns, end_ns, count }], layout_cache: { builds, hits } }`. `bars` lists the first `max_bars` (1–100,000, default 200); `held` is the frame a click holds, or null; `frame` holds one (a `frame` outside the history fails `profiler_no_timeline` and leaves the hold as it was). `frames` (1–8, default the tab's, initially 1) lays that many consecutive frames on one strip axis — the answer's `frames` list each with its start offset, the boundary the panel draws. |
| `editor.profiler_freeze { frame? }` | edit_scene | The History tab's freeze click: freezes history frame `frame` (an index into the shown history, as the bars and hitch rows number them) — the panel highlights the bar and shows that frame's spans, and `editor.profiler_state` reports it as `frozen`. While a frame is frozen the history takes in no new frames (the frozen index and the hitch rows keep naming the same frames); an absent `frame` unfreezes and the history resumes. Answers `{ frozen, history: { frames } }`; out of range is `profiler_bad_frame`. |
| `editor.profiler_units { percent? }` | edit_scene | The **% of frame** toggle of the Flame and Timeline tabs: `percent: true` shows span times as percent of the frame, `false` as ms; absent keeps it. Answers `{ percent, frame_ms, shares: [{ name, ms, percent }], total_percent }`: the source's top-level spans and `(outside spans)` (the rest of the frame), whose percentages sum to 100. `frame_ms` is the frame time, or the root spans' sum when larger. |
| `editor.profiler_modules { module, metric? }` | edit_scene | Shows a module tab (`audio`, `physics`, `ui` or `rendering`) and answers `{ module, source, samples, metrics: [{ key, label, source, summary: { last, min, max, avg, count } }], chart: { metric, values, points } }` over its history (one sample per repaint, up to 300). `metric` picks the charted metric by key. |
| `editor.profiler_monitors { add?, remove?, filter? }` | edit_scene | Shows the Monitors tab: `{ counters: [name], dropped_names, shown: [{ name, summary, samples, total }] }`. `add` charts a counter (at most 8; the oldest leaves), `remove` takes it off, `filter` narrows the list (case-insensitive). |
| `editor.profiler_loads { sort?, descending?, limit? }` | edit_scene | Shows the Loads tab: `{ sort, descending, recorded, dropped, rows_total, rows: [{ id, path, kind, bytes, read_ms, decode_ms, upload_ms, total_ms, refs, source, ok }] }`, sorted by `asset`, `kind`, `size`, `read`, `decode`, `upload`, `total` (the default) or `refs`; `limit` 1–10,000 (default 100). |
| `editor.profiler_marks {}` | read | The marks drawn on the History graph: `{ history: { frames }, marks: [{ index, kind, label, frame }] }`, `kind` `trace`, `level_load` or `hitch`, `index` into the shown history. |

Sources (`kind`):

- `edit`: the editor's own spans. Play frames run inside `pie_frame`, `pie_step` (stepping) and `pie_client_frame` (each
  client world of a several-client session), and every picture the editor draws of the play world (a viewport pane or
  the play window) runs inside `pie_render` (ADR-8594); the edit source is the tree without those subtrees, and its
  frame time is the frame minus their time.
- `pie`: those subtrees, as roots. ECS systems that a multi-threaded schedule runs on a worker thread are filed under the
  play span too: the editor sets a flame context around each play frame (`regia_core::profiling::flame_context`), and
  a root span ending on a compute task-pool thread inside it goes under it. Work on the async-compute and IO pools
  (asset imports, network polls) stays in the edit source even while a play frame runs (ADR-7427). Entity counts are
  the play world's.
- `process` (`address`: `server` or `client <k>`): a child of a separate-process PIE session
  ([PIE with several clients](pie-net.md)). Each child advertises its own endpoint in
  `build/pie/<session>/agent/server` (or `client-<k>`) through `REGIA_AGENT_DIR`; a dedicated server gets `--agent`. A
  child built without the `agent` feature never answers and stays `disconnected`.
- `remote` (`address`: `ws://127.0.0.1:<port>` or `127.0.0.1:<port>`, `token`: the app's `.regia/agent/token`): a dev
  build running on this machine. Other hosts are refused: the token never leaves the machine.

A process or remote source whose polling worker panicked while storing a poll shows `status: "disconnected"` with a
`message` starting `the profiler worker panicked` (ADR-7427), in the panel and in `editor.profiler_state`. To start a
new worker, call `editor.profiler_source` with the source (it always restarts it); in the panel, click the picker's
selected entry while its link is disconnected (ADR-7493), or press **Connect** again for a dev build.

A process or remote source is read by a worker thread: it connects with `regia_agent`'s client, turns call-tree
capture on (`profiler.set_capture`), then every 250 ms reads `profiler.frame` (the 60-frame tree, and with its
`since_frame` cursor every frame that ended since the last poll, for the history) and every fourth time
`memory.report` and the ranked entity counts (`profiler.frame { entities: 20 }`). A build older than these
parameters answers with one sample per poll and entity totals only. `status` is `connecting`, `connected` or
`disconnected` (`message` holds the error; the worker retries every second). The editor reads the worker's last
result without waiting, so a slow or dead process never holds a frame. In-process sources report `live` (`pie` reports
`no_session` while nothing plays).

Tabs:

- **Flame**: the call tree, width ∝ inclusive time, and the GPU passes; an opened capture replaces the live tree.
- **Timeline**: one frame — or a strip of up to eight — of the per-thread timeline (ADR-7325, ADR-7409): a
  block of rows per thread (named on the right), one row per nesting depth, a bar per span at its start and
  end. The **Frames** control (1–8, default 1) lays that many consecutive frames on one time axis with a
  boundary line and the frame number at each frame start; zoom and pan cover the whole strip, ◀ ▶ step it by
  one frame and a hover names the frame a bar belongs to. The wheel (or a pinch) zooms about the pointer, a
  drag pans, and a click holds the shown frame (Live follows the newest again). Bars narrower than a pixel
  next to each other merge into one gray bar labelled with the count, so a row draws at most one bar per
  pixel column; the layout is kept while the frame and the view are unchanged. Record
  timeline turns the in-process profiler's timeline on. `pie` shows the spans inside the play spans' time,
  `edit` the rest; a process or remote source is read by the same worker, which polls `profiler.timeline`
  only while this tab is shown (and turns the process's timeline on the first time).
- **History**: the frame times of the profiler's 300 retained frames (a process source: every frame too, read through
  `profiler.frame`'s `since_frame` cursor). A frame slower than twice the median (and at least 4 ms) is a hitch,
  listed with its five costliest spans. Click a bar or a hitch to freeze that frame and see its spans
  (`editor.profiler_freeze { frame? }` does the same from outside, and `editor.profiler_state` reports the frozen
  index). While frozen the graph takes in no new frames; **Unfreeze** resumes it.
- **Entities**: spawned entities (the same number as `memory.report` `ecs.entities`), archetypes and the 20 components
  held by the most entities. A process source shows the same ranking, from `profiler.frame { entities }` (a build
  older than that parameter: the totals only).
- **Memory**: `memory.report` of the source: every category (subsystem) with its current and peak heap and GPU bytes,
  owners and `assets_by_type`; named snapshots and their diff; and the GPU resource browser with previews. See
  [Memory snapshots and the GPU resource browser](#memory-snapshots-and-the-gpu-resource-browser).
- **Compare**: Set A and Set B open two captures; the table lists every span's change and the flame view of B is colored
  by it (grey unchanged, red slower, green faster).
- **Audio**, **Physics**, **UI**, **Rendering** (modules, P-307): a chart of one metric over the history (the
  **Chart** buttons pick it; Tab, then Space) and a table of every metric's last, min, max and average. The values
  are counters each subsystem keeps, sampled once per repaint from the shown in-process world (the edit world, or
  the play world for `pie`; a process or remote source shows no modules):

  | Module | Metric (key) | Source |
  |---|---|---|
  | Audio | `voices`, `virtual_voices` | `Audio::real_voice_count`, `virtual_voice_count` |
  | Audio | `streaming`, `starved`, `underruns` | `Audio::stream_stats()` |
  | Physics | `step_ms` | `PhysicsWorld::stats().last_step_ms` (wall time of the last rapier step) |
  | Physics | `bodies`, `contacts` | `PhysicsWorld::stats()` `bodies`, `contact_pairs` |
  | Physics | `islands` | distinct persistent islands of the dynamic bodies (rapier's island manager) |
  | Physics | `active_bodies` | `PhysicsWorld::islands.num_active_bodies()` |
  | UI | `nodes` | `UiLayoutStats::nodes_walked` (the last layout run) |
  | UI | `layout_ms` | the frame profiler's `ui_layout` span in the last frame (0 when not recorded) |
  | UI | `draw_batches` | `SharedUiRenderer::batch_count` |
  | Rendering | `gpu_ms`, `passes` | `GpuTimings::frame_ms`, the timed passes (more than 0 ms) |
  | Rendering | `draws`, `pipeline_switches` | `FrameDrawCounts::total()` |
  | Rendering | `meshes_drawn`, `meshes_culled` | `DrawStats::drawn`, `culled` |
  | Rendering | `graph_pipelines`, `pipelines_created` | `GraphPipelineStats::ready`, the process's pipeline creations |

  Every metric is also listed as the telemetry counter `<module>.<key>` (for example `physics.step_ms`), so
  Monitors can graph it next to others. Audio has no mix-time counter yet (Known issues).
- **Monitors** (P-312): every named counter of the telemetry counter listing, filtered by name; Space (or a click) on a
  row puts the counter on the chart or takes it off (up to eight, each in a theme data color), and the table gives
  last, min, max and average. The listing holds the engine's built-in counters and gauges as `core.<name>`, the
  module metrics, and a game's own counters: `regia::telemetry::counters::counter("enemies_alive", n as f64)` keeps
  the newest 300 samples of the name (and records it in a trace's `counters` stream). The listing keeps at most 512
  names (`dropped_names` counts samples of further names).
- **Loads** (P-314): the editor asset server's per-asset load times, one row per asset (its newest load): read,
  decode and upload (the loader's prepare step) ms, cooked bytes, source (`pak` or `cache`), live strong handles.
  Typed loads (`load_mesh`, `load_texture`, ...) have no upload step there (0 ms; their GPU upload is the
  renderer's). Each header button sorts by its column (again: the other way). `AssetServer::load_times()` reads the
  same table (at most 4,096 rows; the oldest load leaves first).

**Marks** (P-318): the History graph draws a line and a marker at the bar of every trace mark
(`trace.mark`, `regia::telemetry::trace::mark`), level load (`LevelLoaded`) and hitch; hovering the bar names them,
and the **Marks** list under the graph (Tab, then the arrow keys) reads the same labels. The Timeline tab draws a
line at the start of the frame a mark was recorded in (a mark has no time inside its frame). Marks are mapped for
the in-process sources; a process source's graph shows its hitches only. Each mark carries the profiler frame it
was recorded in (the frame profiler the panel reads, attached when the panel opens; 0 before that).

The window reads its source once per repaint, inside one `profiler_panel` span.

Editor error codes (`data.code`, invalid parameters):

| Code | When |
|---|---|
| `profiler_bad_kind` | `kind` is not `edit`, `pie`, `process` or `remote`. |
| `profiler_bad_process` | `kind: process` without `address` `server` or `client <k>`. |
| `profiler_bad_address` | `kind: remote` with an address that is not `ws://127.0.0.1:<port>`. |
| `profiler_no_token` | `kind: remote` without `token`. |
| `profiler_no_session` | `kind: process` while no separate-process PIE session runs (or `server` of a listen session). |
| `profiler_worker_failed` | The polling thread could not start. |
| `profiler_bad_top` | `top` is not an integer in 1–200. |
| `profiler_bad_slot` | `slot` is not `view`, `a` or `b`. |
| `profiler_bad_capture` | The file cannot be read or is not a capture. |
| `profiler_no_timeline` | `editor.profiler_timeline` while the source's timeline is off, has no frame yet, or no longer holds the asked `frame` (`data.status` in the message: `timeline_off`, `no_profiler`, `no_frame`, `disconnected`, `refused`). |
| `profiler_bad_max_bars` | `max_bars` is not an integer in 1–100,000. |
| `profiler_bad_frames` | `editor.profiler_timeline`: `frames` is not an integer in 1–8. |
| `profiler_bad_frame` | `editor.profiler_timeline`: `frame` is not an integer. `editor.profiler_freeze`: `frame` is not an integer, or not in the shown history. |
| `profiler_bad_percent` | `editor.profiler_units`: `percent` is not a boolean. |
| `profiler_bad_module` | `editor.profiler_modules`: `module` is not `audio`, `physics`, `ui` or `rendering`. |
| `profiler_bad_metric` | `editor.profiler_modules`: `metric` is not one of the module's metric keys. |
| `profiler_bad_counter` | `editor.profiler_monitors`: `add` or `remove` is not a string. |
| `profiler_unknown_counter` | `editor.profiler_monitors`: `add` names a counter that never reported. |
| `profiler_bad_sort` | `editor.profiler_loads`: `sort` is not a column key. |
| `profiler_bad_limit` | `editor.profiler_loads`: `limit` is not an integer in 1–10,000. |

```json
{"method":"editor.pie_net_config","params":{"mode":"dedicated","clients":1,"separate_processes":true}}
{"method":"editor.play","params":{}}
{"method":"editor.profiler_source","params":{"kind":"process","address":"server"}}
{"method":"editor.profiler_state","params":{"top":5}}
{"method":"editor.profiler_capture_slot","params":{"slot":"a","path":"captures/before.rprof.json"}}
{"method":"editor.profiler_capture_slot","params":{"slot":"b","path":"captures/after.rprof.json"}}
```

## Memory snapshots and the GPU resource browser

The Profiler's **Memory** tab has two halves (ADR-8142), switched at its top.

**Categories and snapshots.** One row per category (engine subsystem): live heap bytes and their peak since start-up
(the `regia_core::memory` ledger; both `-` without the tagging allocator), GPU bytes of the allocations labeled for it
and the highest GPU bytes seen while the editor ran. **Take snapshot** records, under the name in the field (empty:
`Snapshot <n>`), the category rows, the entity count of every archetype and of every component, and the loaded count
and bytes of every asset kind. The list keeps the 32 newest; Up and Down move through it, **Diff from** and **Diff to**
pick the focused snapshot as a side (by default the two newest are compared). The diff lists the categories that
changed, most heap plus GPU growth first, and, as the object-leak view, the archetypes, components and asset kinds
whose counts changed, most growth first: N entities spawned between two snapshots read as exactly `+N` in their
archetype. **Export JSON** writes the focused snapshot to `captures/memory/<name>.json` under the project (a report:
the editor never reads it back). In-process sources (`edit`, `pie`) are snapshotted from this editor's world and
process; a `process` or `remote` source from the last `memory.report` the tab read from it, without archetype and
component rows.

**GPU resources.** Every texture and render target of the renderer's registry (`seeing.list_resources`; a render
target is one with `RENDER_ATTACHMENT` usage) with its format, usage, size, mips, layers and bytes, and every other live
GPU allocation of the allocator report by label (`allocation`: buffers and textures the registry does not track, with
their bytes and count), each with its owner (the engine subsystem of its label) and memory category, largest first.
Up and Down step through the list; the selected texture's preview follows: the chosen mip and layer copied back from
the GPU (`regia_render::read_texels`), point-sampled to at most 256 texels on its longer side, all channels or one as
gray, values clamped to 0–1 (sRGB formats show as stored). **Previous mip** and **Next mip** step the mip, the channel
buttons pick RGBA, R, G, B or A, **Live preview** reads it again every 30 frames and **Refresh preview** once (with the list, which is otherwise
read again every 30 frames). A
preview needs a render device and a target created while debug readback was on (the agent plugin turns it on);
multisampled and depth-stencil targets list without a preview.

| Method | Permission | What it does |
|---|---|---|
| `editor.memory_snapshot { action, name?, a?, b?, path? }` | edit_scene | `take` records a snapshot of the Profiler's source (`name` 1–64 bytes, default `Snapshot <n>`): `{ snapshot }` with `name`, `index`, `source`, `heap_tracked`, `heap_live_bytes`, `heap_peak_bytes`, `gpu_allocated_bytes`, `entities`, `categories: [{ name, heap_bytes, heap_peak_bytes, heap_allocations, gpu_bytes }]`, `archetypes` and `components: [{ name, entities }] \| null`, `assets: [{ kind, loaded, bytes }] \| null`. `list`: `{ snapshots: [{ name, index, source, entities, heap_live_bytes, gpu_allocated_bytes }], diff: { a, b } \| null }`. `diff` compares `a` with `b` (both absent: the tab's sides, else the two newest) and makes them the tab's sides: `{ diff: { a, b, heap_bytes, gpu_bytes, entities, categories: [{ name, heap_bytes, heap_allocations, gpu_bytes }], archetypes, components, assets: [{ name, a, b, delta, bytes }] \| null } }`, only changed rows, most growth first. `export` writes snapshot `name` as JSON to `path` (project-relative, ending in `.json`; default `captures/memory/<name>.json`): `{ path }`. |
| `editor.gpu_resources { action, top?, label?, mip?, layer?, channel?, max_size?, pixels? }` | read | `list`: `{ backend, total_bytes, count, resources: [{ label, kind, format, usage, width, height, layers, mips, bytes, count, owner, category, previewable }] }`, largest first, the first `top` (1–5000, default 200); `kind` is `texture`, `render_target` or `allocation`. `preview` reads mip `mip`, layer `layer` of texture `label`, point-sampled to at most `max_size` (1–256, default 64) on its longer side, channel `rgba`, `r`, `g`, `b` or `a`, and shows it in the panel: `{ label, mip, layer, channel, width, height, source_width, source_height, pixels: [[r, g, b, a]] \| null }` (`pixels: true` answers the RGBA8 pixels, row-major). |

Error codes (`data.code`):

| Code | When |
|---|---|
| `memory_bad_action` | `action` is not `take`, `list`, `diff` or `export`. |
| `memory_bad_name` | A snapshot name that is empty, longer than 64 bytes or has control characters. |
| `memory_name_taken` | `take` with the name of a snapshot in the list. |
| `memory_unknown_snapshot` | No snapshot of that name (or `diff` with only one of `a`, `b`, or fewer than two snapshots). |
| `memory_no_report` | A `process` or `remote` source before the tab read its first `memory.report`. |
| `memory_bad_path` | `export` with an absolute path, `..`, or a path not ending in `.json`. |
| `memory_no_project` | `export` while no project is open. |
| `memory_write_failed` | The file could not be written. |
| `gpu_bad_action` | `action` is not `list` or `preview`. |
| `gpu_bad_top` | `top` is not a non-negative integer. |
| `gpu_label_required` | `preview` without `label`. |
| `gpu_bad_subresource` | `mip` or `layer` is not a non-negative integer. |
| `gpu_bad_max_size` | `max_size` is not an integer in 1–256. |
| `gpu_bad_channel` | `channel` is not `rgba`, `r`, `g`, `b` or `a`. |
| `gpu_no_device` | `preview` in an editor without a render device (headless). |
| `resource_not_found`, `resource_not_copyable`, `format_unsupported`, `subresource_out_of_range`, `readback_failed` | The resource inspector refused or failed the preview ([the resource inspector](resource-inspector.md)). |

```json
{"method":"editor.memory_snapshot","params":{"action":"take","name":"before load"}}
{"method":"editor.memory_snapshot","params":{"action":"take","name":"after unload"}}
{"method":"editor.memory_snapshot","params":{"action":"diff","a":"before load","b":"after unload"}}
{"method":"editor.gpu_resources","params":{"action":"list","top":20}}
{"method":"editor.gpu_resources","params":{"action":"preview","label":"hdr color","channel":"a","max_size":32}}
```
