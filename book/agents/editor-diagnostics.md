# Editor diagnostics: the GPU tab and the trace timeline (agents)

ADR-2771. Spec: `docs/specs/54-gpu-breadcrumbs-and-trace-markers.md` § 6. Code: `regia_editor::{diagnostics_panel,
trace_timeline, diagnostics_agent}`.

View ▸ Diagnostics opens a window with two tabs. It shows what the agent methods of
[GPU breadcrumbs and trace markers](gpu-breadcrumbs-and-trace.md) return, so a person can look at the same evidence an
agent reads. The window calls the registered `render.breadcrumbs` and `telemetry.trace` handlers; there is no second
reader of the recorder.

| Tab | Shows |
|---|---|
| GPU | the evidence level (`none`, `recorded`, `fenced`, `timestamped`) and the adapter; the counters `submitted`, `completed`, in flight (`submitted - completed`) and the acknowledged frame; the suspect passes of a loss report; the newest frame's pass list in GPU order; a **Load last crash bundle** button |
| Timeline | a `.regiatrace` as lanes: marks as vertical lines, ranges as bars (one row per nesting depth, one lane per `main`, `render`, `worker:<n>`, `gpu`), GPU passes on the `gpu` lane, screenshot thumbnails on a `screenshots` lane at the top; zoom and pan; a **Record** button |

Hovering a bar shows its name, lane, frame and duration. **Open newest** loads the newest `.regiatrace` of the project's
`profiling` folder (else the working directory's). **Record** starts the process recorder (`telemetry.trace` `start`,
file `editor-<unix seconds>`); the button turns into **Stop recording**, and stopping opens the written file.

## Load last crash

The button (and `editor.diagnostics { load_crash: true }`) reads the newest `crash-*.txt` crash report in the crash
directory that has a `gpu breadcrumbs:` block and shows that block with the report's first line. Without one it reads
the newest crash bundle's `gpu_breadcrumbs.json` (`render.breadcrumbs { source: "last_bundle" }`) and shows the same
text. With neither it says where it looked.

## Methods

| Method | Permission | Params | Result |
|---|---|---|---|
| `editor.diagnostics` | `edit_scene` | `open?` (bool), `tab?` (`gpu`, `timeline`), `load_crash?` (bool) | `{ open, tab, gpu: { level, level_number, adapter, submitted, completed, in_flight, acknowledged_frame, last_frame, last_frame_passes, suspects } \| { unavailable }, crash: { source, headline, block } \| null, crash_error?, status }` |
| `editor.trace_timeline` | `edit_scene` | `path?`, `zoom?` (1 to 10000), `pan?` (0 to 1), `width?` (points), `layout?` (default true), `close?` | `{ file, loaded, events, truncated, warning, adapter, lanes: [{ name, first_row, rows }], zoom, pan, width, items?: [{ kind, name, lane, row, frame, x, width, file, count }], items_total?, status }` (`count` > 1: sub-pixel ranges of a row merged into one bar, ADR-7325) |
| `editor.trace_record` | `play` | `action` (`start`, `stop`, `status`), `file?` | the `telemetry.trace` summary, plus `opened_in_timeline` on `stop` |

`gpu.unavailable` carries the reason when breadcrumbs are off or the app has no renderer. `path` is relative to the
open project, or an absolute path under the project or the temp directory; anything else, `..` or a symlink leading
out is `path_outside_project`. Opening a trace shows the window's Timeline tab. `items` are ordered by frame, then time, then file
order; `kind` is `mark`, `range`, `gpu_pass` or `screenshot`; `x` and `width` are points in a view `width` wide (0 width
for marks and screenshots). At most 500 items are listed; `items_total` is the count in view. Zoom 1 shows the whole
trace; at zoom `z` the view shows `1/z` of it, starting `pan` of the way through the rest. Items wholly outside the view
are left out and bars are clipped to it.

```json
{"method": "editor.trace_record", "params": {"action": "start", "file": "boss"}}
{"method": "telemetry.mark", "params": {"name": "boss door opened"}}
{"method": "editor.trace_record", "params": {"action": "stop"}}
{"method": "editor.trace_timeline", "params": {"zoom": 4, "pan": 0}}
```

A file that is empty, not a trace, or from a newer format version is refused with `trace_unreadable` (the message says
which); the trace already open stays. A file cut short inside a chunk loads, with `warning` and `truncated: true`, showing
the events before the cut.

## Errors

| Code | Meaning |
|---|---|
| `bad_tab` | `tab` is not `gpu` or `timeline` |
| `bad_view` | `zoom`, `pan` or `width` is not a number |
| `bad_path` | `path` is empty |
| `path_outside_project` | `path` is outside the project and the temp directory, has `..`, or a symlink in it leads out (`data.path_reason` says which) |
| `trace_unreadable` | the file cannot be read or decoded (empty, bad magic, newer version) |
| `bad_action` | `action` is not `start`, `stop` or `status` |
| `bad_param` | `open` or `load_crash` is not a boolean, or `file` is not a string |
| `no_recorder`, `trace_busy`, `trace_not_writing`, `bad_file_name`, `trace_io` | the `telemetry.trace` reasons ([GPU breadcrumbs and trace markers](gpu-breadcrumbs-and-trace.md)) |
