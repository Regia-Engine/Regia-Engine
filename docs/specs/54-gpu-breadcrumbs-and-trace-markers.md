# Spec 54: GPU breadcrumbs and trace markers

Status: proposed (ADR-1579). Target rung: v0.2.x (breadcrumb ring, crash-bundle entry, marks, agent methods)
and v0.3.x (ranges, screenshot marks, timestamp-backed breadcrumbs, trace export).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_render::gpu_health`, `regia_render::device::TrackedQueue`, `regia_render::frame`
(`FrameRenderer::render_into`, `GpuTimings`, `FrameCpuTimings`), `regia_framework::feedback::crash`,
`regia_telemetry::{gpu, phase}` (§ 2.1).
Crates: `regia_render` (ring, pass scopes, submission counters), `regia_framework::feedback` (bundle entry),
`regia_telemetry` (trace recorder and file), `regia_cli` (`regia trace export`), `regia_agent` (methods).
Source area: runtime-rhi/runtime-rhi-gpu-diagnostics, developer-insights/developer-insights-trace-capture
(vocabulary only: developer-insights/developer-insights-timing-analysis).

## 1. Purpose and scope

**Problem.** When the GPU device is lost (driver reset, watchdog timeout, page fault), the CPU learns about it
late, at an unrelated call site; the CPU stack at that point says nothing about which pass faulted. Today a lost
device produces a crash bundle with the reason, wgpu's message, error counts, GPU memory and the last frame
summary, and nothing about *where in the frame* the GPU was. Separately, developers and agents cannot annotate
a timing recording ("the boss door opened here", "begin foliage stress test") or line up screenshots with it.

**This spec adds**

1. *Breadcrumbs*: a small ring of per-frame pass markers recorded around every GPU pass, ordered as the GPU
   receives them, plus submission counters, so a device-lost bundle names the passes the GPU was most likely
   inside and the pass history of the last frames.
2. *Trace marks*: named instants (marks), named nestable intervals (ranges) and screenshot marks written to a
   lightweight trace recorder with a stable file format and an export to a public trace-viewer format.
3. *Agent methods* to read breadcrumbs, add marks, run a recording, and provoke a device loss in tests.

**Not in scope**: vendor post-mortem SDKs (§ 11), a GPU debugger or frame capture, a CPU sampling profiler
(the existing Tracy feature of `regia_core` stays the live deep profiler), the editor profiler UI, preventing
device loss. The device-lost policy is unchanged: stop drawing, write the bundle, exit with
`DEVICE_LOST_EXIT_CODE` (3).

## 2. Model

Terms: a *pass* is one named GPU pass of the frame (a `wgpu` render or compute pass, or a group of them under
one name); a *segment* is one command buffer recorded by the frame thread or an encode worker; a *submission*
is one `queue.submit` call.

### 2.1 Baseline (implemented, not respecified)

- `regia_render::gpu_health` (ADR-171): process-wide device-lost flag, `DeviceLostInfo { reason, message }`
  with `kind()` and `exit_code()`, uncaptured-error and out-of-memory counters, `note_exiting()` so a destroy
  during shutdown is not a loss (`counts_as_loss(reason, exiting)`), `DEVICE_LOST_EXIT_CODE = 3`.
- `regia_render::DeviceLostHooks`: run once on a loss. The feedback plugin's hook calls
  `regia_framework::feedback::crash::write_device_lost_bundle(&DeviceLostInfo)`, which writes a feedback bundle
  (`.zip`, `BUNDLE_FORMAT = 1`) with `crash.txt` and `host.json` entries. `regia_crash` only uploads reports;
  it writes nothing.
- `regia_telemetry::gpu::install`: device error and loss callbacks emit `regia::gpu_error` and
  `regia::device_lost` tracing events. `regia_telemetry::phase(name, data)` emits `regia::phase` session
  transitions (spec 28 § 3).
- The frame is recorded by `FrameRenderer::render_into` (`regia_render::frame`), not through `RenderGraph`:
  about 50 pass sites, each with an `info_span!`; encode workers record segments in parallel; all submits go
  through `TrackedQueue` (`regia_render::device`), and `FrameCpuTimings.submits` counts them (1 in a steady
  frame). `RenderGraph::execute` (`regia_render::graph`) runs plugin graph nodes into one encoder.
- `GpuTimings` (named, grouped GPU timings) exist when the device has `TIMESTAMP_QUERY`;
  `regia_render::capabilities` reports `timestamp_query` (needs `TIMESTAMP_QUERY` and
  `TIMESTAMP_QUERY_INSIDE_ENCODERS`).
- Crash CRC-32 helper: `regia_framework::feedback::zip::crc32`. `lz4_flex` is already a workspace dependency
  (pak compression, `regia_asset`).

### 2.2 Breadcrumb model (new)

- **Pass names.** Every pass site opens a scope with a `&'static str` name (`"shadow_cascades"`, `"gbuffer"`,
  ...), the same name as its `info_span!`. Names are interned once into a process-wide, append-only table
  (`PassTag(u16)` -> name, at most 1,024 names; the 1,025th name maps to the tag `overflow`). Graph node ids
  (`RenderGraphNode`, `String`) are interned when the graph compiles. Tags are stable for the process life;
  reports always carry names, never bare tags.
- **Markers.** A scope writes `Open(tag)` when the pass begins recording and `Close(tag)` after it ends. Markers
  go into the **segment's own list** (`CrumbList`, preallocated, owned by whoever records that segment), so
  encode workers never share a write target.
- **Ordering by submission.** When `TrackedQueue` submits, it appends the lists of the submitted command
  buffers to the frame record **in buffer order** and stamps them with the submission index. The record is
  therefore in GPU order, not CPU recording time, and only the submit path (frame thread) writes it.
- **Frame record** (`FrameCrumbs`): `frame: u64`, `markers` (up to `MAX_MARKERS = 512`; a frame with about
  150 passes uses 300), `submissions: [(SubmissionId, marker range)]`, `truncated: bool`. Overflow keeps the
  oldest markers of the frame (they ran first), drops the rest, and sets `truncated`.
- **Ring.** `BreadcrumbRing` holds the newest `ring_frames` records (default 8, range 2-64). Decision
  (ADR-1585): each slot is a `std::sync::Mutex<FrameCrumbs>`; the submit path locks one slot once per frame to
  publish a finished record (a copy from the frame-local buffer; as built, ADR-5640, the frame's slot is
  republished at every submit, so a frame with several submits copies its record once per submit and a reader
  sees the frame's submissions so far); readers `try_lock` with a short bounded retry
  (the pattern of `feedback::crash::clone_context`) and skip a slot they cannot get. No `unsafe`, no seqlock,
  no new dependency.
- **Submission counters** (CPU side, portable), relaxed atomics:
  - `submitted`: incremented after each `queue.submit` returns.
  - `completed`: from `Queue::on_submitted_work_done`, one callback per submission carrying its index. Callbacks
    may arrive out of order; `completed` is the highest index such that it and every earlier index have
    completed (a 64-entry bitmap window). Duplicate completions are ignored.
  - `acknowledged_frame`: the newest frame whose last submission completed.
  `submitted - completed` is the in-flight depth (normally 1-3).
- **Evidence levels** (what a report may claim, best available first):

| Level | Name | Needs | The report can say |
|---|---|---|---|
| 3 | `timestamped` | `timestamp_query` | The last pass whose closing timestamp resolved before the loss; the passes after it are suspects |
| 2 | `fenced` | core wgpu | Submissions up to S completed, S+1..T were in flight; the passes of those submissions are the suspects |
| 1 | `recorded` | nothing | CPU pass order only; the suspects are the whole last frame |
| 0 | `none` | writer disabled | Frame index and adapter only (today's bundle) |

A report states the achieved level and never implies more.

### 2.3 Timestamp backing (v0.3.x, optional)

With `timestamp_query`, scopes also write one GPU timestamp at open and close into a query set (2 slots per pass
per frame slot, `frames_in_flight + 1` sets), resolved into a mapped readback buffer. A hung device never
returns its readback, so level 3 uses *earlier completed frames* plus the loss frame's last resolved slice; the
loss frame itself is bracketed by level 2. Where `GpuTimings` already writes timestamps, the breadcrumbs reuse
its query set (one set, two readers; timing owns the resolve); without timing, breadcrumbs create their own
coarse set. No vendor extension, no GPU-written host memory and no fault address is assumed. A backend that
gives a device-lost message has the text copied verbatim.

### 2.4 Trace recorder (new)

- **Recorder states.** `Idle` (nothing kept), `Windowed` (a rolling in-memory window of the newest events, no
  file), `Writing` (events go to a file; the window keeps filling). Transitions:

| From | Trigger | To | Effect |
|---|---|---|---|
| `Idle` | boot with `window_seconds > 0` | `Windowed` | window allocated |
| `Idle` or `Windowed` | `telemetry.trace { action: "start" }` or launch argument `--record-trace` | `Writing` | file opened, header written; from `Windowed`, the window is written first so the file starts before the call |
| `Writing` | `action: "stop"` | `Windowed` (or `Idle` with no window) | file closed, summary returned |
| `Windowed` or `Writing` | `action: "save_window"` | unchanged | the window is written to a separate file; a running file is not touched |
| `Writing` | write error | `Windowed`, flag `degraded` | one error log; the file is closed at its last complete chunk |

- **Mark**: `{ name, t_ns, frame, lane, data? }`, an instant for rare, meaningful moments. At most 100 per
  second per name; more are dropped and counted.
- **Range**: `{ id, name, begin_t_ns, end_t_ns, frame_begin, frame_end, lane, depth, parent? }`, opened and closed
  by id. Nesting is per lane. Closing a range that is not the innermost open one on its lane closes the inner
  ones first, flagged `forced_close`; closing an unknown id is ignored and counted; ranges open at stop are
  closed with `forced_close`. At most 32 deep per lane; deeper opens are dropped and counted.
- **Screenshot mark**: `{ name?, t_ns, frame, width, height, file }`: the PNG is written next to the trace file
  (`<trace>.shots/<n>.png`) and referenced, never inlined.
- **Lanes**: `main`, `render`, `worker:<n>`, `gpu` (pass intervals from breadcrumbs, engine-written only).
  Agents and game code open ranges on CPU lanes only.
- **Streams** (what is recorded): `marks` (marks, ranges, screenshot marks; always on while writing), `frames`
  (frame begin/end), `passes` (breadcrumb pass intervals, about 150 per frame), `counters`, `logs` (opt-in; reserved: the stream is selectable but no producer feeds it yet, so a file with it holds no log events).
  Default for `start`: `marks`, `frames`, `passes`. A disabled stream costs one relaxed load.
- **Time base**: monotonic nanoseconds since process start. Pass events recorded from breadcrumbs are
  stamped with the CPU time of the call and carry GPU submission order, not GPU time; `gpu_clock_error_ns` in the
  header stays 0 until a producer resolves GPU timestamps into `GpuInterval` events (planned: convert with the queue's
  timestamp period, re-base with one CPU/GPU sample per second, store the error bound in the header).
- Each mark also emits a `regia::mark` tracing event (`name`, `frame`), so hosts with the JSON log layer see it
  without a trace file. `regia::phase` stays the session-transition event and is recorded as a `phase:<name>` mark too.

## 3. Rust API

Layering: the ring, scopes and counters live in `regia_render` (it owns the device and the frame); the bundle
entry is written by `regia_framework::feedback` (Gameplay layer, already depends on `regia_render`); the recorder
lives in `regia_telemetry` (Gameplay layer, already depends on `regia_render` and `regia_framework`, an allowed
same-layer edge); `regia_agent` calls read-only accessors and the recorder. No new crate edge. `regia_crash` is
untouched.

```rust
// regia_render::breadcrumbs
pub struct PassTag(pub u16);
pub struct SubmissionId(pub u64);
pub fn enabled() -> bool;                                     // one relaxed load
pub fn configure(c: BreadcrumbConfig);
pub fn pass(list: &mut CrumbList, name: &'static str) -> PassScope<'_>;  // Open now, Close on drop
pub fn intern(name: &str) -> PassTag;                         // graph node ids at compile
pub fn note_submit(lists: &[&CrumbList], frame: u64) -> SubmissionId; // called by TrackedQueue::submit
pub fn note_completed(id: SubmissionId);                      // from on_submitted_work_done
pub fn snapshot() -> BreadcrumbSnapshot;                      // copy of ring + counters for readers
pub struct BreadcrumbConfig { pub enabled: bool, pub ring_frames: u8, pub timestamps: TimestampMode }
pub enum TimestampMode { Off, WhenSupported }

#[derive(Serialize, Deserialize)]                  // no Reflect: regia_render has no bevy_reflect (ADR-1671 (5))
pub struct BreadcrumbReport {
    pub format_version: u32,                  // 1
    pub level: EvidenceLevel,                 // timestamped | fenced | recorded | none
    pub adapter: AdapterSummary,              // name, backend, device type, driver, driver info, vendor/device ids
    pub lost: Option<LostSummary>,            // reason, message, frame at callback, unix ms
    pub counters: SubmissionCounters,         // submitted, completed, acknowledged_frame
    pub suspects: Vec<SuspectPass>,           // ordered: name, frame, submission
    pub frames: Vec<FrameReport>,             // newest first
    pub truncated: bool,
}
pub enum EvidenceLevel { Timestamped, Fenced, Recorded, None }
impl BreadcrumbReport { pub fn capture(snapshot: &BreadcrumbSnapshot, lost: Option<&DeviceLostInfo>) -> Self; }

// Test seam in regia_render::breadcrumbs (fake behind cfg(test) and the `test-hooks` feature):
pub trait GpuProgressSource: Send + Sync {
    fn on_work_done(&self, id: SubmissionId, f: Box<dyn FnOnce() + Send>); // wgpu: Queue::on_submitted_work_done
    fn timestamp_period_ns(&self) -> Option<f32>;
    fn resolved_timestamps(&self, frame_slot: u32) -> Option<Vec<u64>>;
}
pub struct WgpuProgress; pub struct ScriptedProgress;

// regia_telemetry::trace
pub fn mark(name: &str, data: Option<serde_json::Value>);     // no-op (one load) when no stream wants it
pub fn range(name: &'static str) -> RangeGuard;               // RAII; Drop closes
pub fn range_open(name: &str, lane: Lane) -> RangeId;
pub fn range_close(id: RangeId) -> RangeClosed;               // duration, forced_close ids
pub fn screenshot_mark(name: Option<&str>, frame: u64, png: &Path);
pub enum RecorderState { Idle, Windowed, Writing }
pub struct TraceConfig { pub window_seconds: f32, pub window_bytes: usize, pub dir: PathBuf, pub streams: Streams }
pub fn read_trace(path: &Path) -> Result<TraceFile, TraceReadError>;   // used by the CLI export and tests
```

- **Plugins.** `BreadcrumbsPlugin` (render group) and `TracePlugin` (telemetry group). Defaults: breadcrumbs on
  in every build profile; recorder `Idle` until `trace::configure` or `start` (a host may configure a `Windowed` 5 s / 2 MiB
  recorder; no plugin does it by default, so a device loss before the first `start` has no pre-start window).
- **Scopes at pass sites.** Each of the ~50 pass sites in `render_into` gets a `breadcrumbs::pass(...)` scope
  next to its `info_span!`; `RenderGraph::execute` opens one scope per node. `TrackedQueue::submit` calls
  `note_submit` and registers the completion callback. A frame-path pass without a scope is a review defect;
  test 17 checks that the shadow, opaque, post and UI passes of a sample frame appear.
- **Bundle entry.** `write_device_lost_bundle` adds `gpu_breadcrumbs.json` (`BreadcrumbReport`) and a
  `gpu breadcrumbs:` block in `crash.txt` (level, suspects, counters, the last 3 frames one line each), and
  lists the entry in `feedback.json`'s present parts.
- **Pass names are a contract.** Reports carry names; renaming a pass is a release-note line.

## 4. Files and formats

### 4.1 Bundle entry `gpu_breadcrumbs.json`

`BreadcrumbReport` with `"format_version": 1`, at most 64 KiB (8 frames x 150 passes fits; on overflow the
newest frames are kept and `truncated` set). The feedback bundle stays `BUNDLE_FORMAT = 1`: an added optional
zip entry is additive (readers list entries and ignore unknown ones). The entry is also written, at most once
per minute, into a category-`bug` feedback bundle of an uncaptured validation or internal error (`lost: null`,
including when the evidence level is `none`). That write does not take the crash-bundle slot. Out of memory
does not write it. The ring then shows what ran just before.

### 4.2 Trace file `.regiatrace`

Binary, little-endian, chunked and append-only: a truncated file reads up to its last complete chunk.

```
Header   { magic: b"RGTR", format_version: u32 = 1, flags: u32, start_unix_ms: u64,
           gpu_clock_error_ns: u32, engine_version: str, adapter: str, build_profile: str, platform: str }
Chunk*   { kind: u8, len: u32, payload: [u8; len], crc32: u32 }   // kind high bit = LZ4 block payload
Kinds    1 Strings (id varint, len varint, utf8)*          ids start at 1; appended before first use
         2 Lanes (id varint, kind u8, n varint, name sid)*
         3 Events (batch: kind u8, t_ns delta varint, lane varint, fields)
         4 Counters (name sid, t_ns varint, f64)
         5 Passes (tag varint, name sid)*                 the reader narrows the tag to u16
         6 Screenshot (t_ns, frame, width, height, name sid or 0, file sid)    varints
         7 Index (second varint, file offset varint)*     written at close
Events   1 Mark{name sid, data sid or 0, frame}
         2 RangeOpen{id, name sid, frame, depth, parent+1 or 0}
         3 RangeClose{id, frame, forced}
         4 FrameBegin{frame}   5 FrameEnd{frame}
         6 PassOpen{tag, frame, submission}   9 PassClose{tag, frame, submission}
         7 GpuInterval{tag, begin_ns, end_ns, frame}
         8 Log{level, target sid, text sid}
```

The same list is `docs/formats/regiatrace-v1.md`. A reader that stops after an older field list
(`Mark{name,data?}`, `RangeOpen{id,name}`, `RangeClose{id,forced}`, or a raw `u16` pass tag) treats
the next varints as the next event code.

Payloads above 1 KiB are LZ4-compressed when that shrinks them (`lz4_flex`, already a workspace dependency;
`regia_telemetry` adds it and the docs/03 row gains `telemetry`). CRC-32 reuses
`regia_framework::feedback::zip::crc32`. Default name `trace-<unix_ms>.regiatrace` in the project's
`profiling/` log directory; on Android, the app's external files directory so `adb pull` works.

### 4.3 Export

`regia trace export <file.regiatrace> --format jsonl|trace-event` (new `regia_cli` command; it reads the file with
`regia_telemetry::trace::read_trace`, a new direct `regia_cli -> regia_telemetry` dependency, Tools over Gameplay,
allowed by the layer order) writes (a) JSON
lines, one object per event with names resolved and the same `format_version`, for the planned editor profiler,
and (b) the public Trace Event Format (`traceEvents` array; `B`/`E`/`i`/`X` phases) for any generic viewer: marks
become global instants, ranges `B`/`E` pairs on their lane, GPU intervals `X` events on a `gpu` lane, screenshot
marks instants with the PNG path in `args`. Decision (ADR-1585): the export stays in this spec; it is small
and makes the file usable before an editor profiler exists.

### 4.4 Versions

Both formats start at `format_version: 1` and are registered with `cargo xtask check-formats` as
`gpu-breadcrumbs` and `regiatrace` (the lead runs `cargo xtask check-formats --update` and `cargo xtask
manual`). Additive changes keep the version (readers ignore unknown JSON keys and skip unknown chunk kinds by
`len`). Decision (ADR-1585): both are diagnostic outputs, not project content, so they are not rewritten by
`regia upgrade-project`; instead any layout change bumps the version and the reader keeps a decoder for every
older version (a checked-in v1 fixture under `crates/regia_telemetry/tests/data/` must always open). Crash
bundles are never rewritten.

## 5. Agent surface

Registered through `AgentExtension` by `regia_agent::breadcrumbs::install_breadcrumb_methods` and
`regia_agent::trace::install_trace_methods`. Classes map to `Permission::Read` / `Permission::Play`.

| Method | Class | Params | Result |
|---|---|---|---|
| `render.breadcrumbs` | read | `{ frames?: u8 (default 4, max ring), passes?: bool (true), source?: "live" \| "last_bundle" }` | `{ source, level_number, report }` where `report` is a `BreadcrumbReport` from the live ring, or parsed from the newest bundle. `level_number` is `report.level` as an integer. `level`, `frames` and `format_version` are fields of `report` |
| `telemetry.mark` | play | `{ name: string (1-64), data?: object (<= 1 KiB) }` | `{ t_ns, frame, recorded }` |
| `telemetry.range_open` | play | `{ name: string, lane?: "main" \| "render" \| "worker:<n>" }` | `{ id, depth, recorded }` |
| `telemetry.range_close` | play | `{ id }` | `{ duration_ns, forced_close: [id] }` |
| `telemetry.trace` | play | `{ action: "start" \| "stop" \| "save_window" \| "status", streams?: [..], window_seconds?, file?: bare name }` | `{ state, file?, bytes, events, dropped, ranges_open, degraded }` |
| `telemetry.screenshot_mark` | play | `{ name?, size?: [w, h] }` | `{ file, frame, t_ns, recorded, width, height, mime, png_base64 }` |
| `render.fault_inject` | play, `test-hooks` builds only | `{ kind: "device_lost" \| "stall_at_submission" \| "oom", submission?: u64, at_frame?: u64, mode?: "scripted" \| "destroy" }` | `{ armed: true, fires_on_frame }` |

As built (ADR-5640): `render.breadcrumbs` takes `frames` from 1 to 64 (a larger value than the ring holds returns
every retained frame; 0 or 65 is `invalid_value`); `telemetry.trace` always carries `file`, `null` when no file is
open; `render.fault_inject`'s `fires_on_frame` echoes `at_frame` (0: the next submit); `test_hooks_off` is returned
for `mode: "destroy"` when there is no render device to destroy (without the feature the method is not registered
and the call is an unknown method). Extra reasons the methods return, all in the book page: `data_too_large`,
`invalid_lane`, `trace_io`, `no_renderer`, `screenshot_failed`, `invalid_value`, `unknown_field`.

`recorded` is false when the mark or range was not stored. `id` 0 with `recorded: false` is the
not-recorded range (the marks stream is off, or the lane is already 32 deep). Real ids start at 1.
`degraded` is true when a write error, including a writer queue past its held-byte cap, closed the
file at its last complete chunk. `telemetry.screenshot_mark` puts the PNG in the JSON result as
`png_base64`. The MCP image-content block is not attached for this method.

Errors follow `regia_agent::post_volumes`: `F0002` (bad params) or `F0003` (not found) with `data.reason` one of
`breadcrumbs_off`, `unknown_range`, `name_too_long`, `trace_busy` (start while writing), `trace_not_writing`,
`bad_file_name` (separators or traversal; files always land in the profiling directory), `no_bundle`,
`test_hooks_off`, `unknown_pass`. `render.fault_inject` exists only when the `test-hooks` feature is compiled in
(`regia_agent` gains a `test-hooks` feature forwarding to the existing `regia_render/test-hooks`) and needs the
play permission; shipping builds never contain it.

Book page `book/agents/gpu-breadcrumbs-and-trace.md`: reading an evidence level; the "mark, run, record, read
back" recipe (`telemetry.trace start`, `telemetry.mark`, `play.step 30`, `telemetry.trace stop`); provoking a loss
with the scripted source or `destroy` on lavapipe and reading the bundle; every reason. `book/COVERAGE.md`: one
§ 2 row per method (`| render.breadcrumbs | render_breadcrumbs | [GPU breadcrumbs](agents/gpu-breadcrumbs-and-trace.md) |`,
...), a § 3 row for `regia trace export`, § 4 rows for both formats. `play.wait_until` can poll the counters in
`render.breadcrumbs`; `play.expect` on `report.level` checks the machine class. `level_number` is the integer beside `report`.

### 5.1 Provoking a loss in tests

- **Scripted source (primary).** `ScriptedProgress` replaces `WgpuProgress`: complete submissions normally,
  stop completing at submission S (a stall), fire the loss with a chosen reason and message, return canned
  timestamps ending at pass P. Everything above the seam (scopes, counters, report, bundle writer, hooks, exit
  code) is the real code. `render.fault_inject { mode: "scripted" }` arms it. As built (ADR-5640): the unit tests
  drive a `Breadcrumbs` instance with `ScriptedProgress`; in a running app `render.fault_inject` arms the
  process-wide `breadcrumbs::fault` plan (`test-hooks` only), which the tracked queue's submit path fires: a loss
  through `gpu_health::mark_device_lost`, a stall that drops the completions from submission S on, or an
  out-of-memory pressure signal. `TrackedQueue` calls `Queue::on_submitted_work_done` itself (`WgpuProgress` is the
  same call behind the trait).
- **Destroy on a software adapter.** `mode: "destroy"` calls `Device::destroy()` at the chosen frame, which
  raises wgpu's real loss callback. Decision (ADR-1585): `gpu_health` gains `note_injected_destroy()` (compiled
  only with `test-hooks`), which makes `counts_as_loss` treat that one destroy as a loss even though wgpu reports
  it as destroyed. This proves the plumbing, not driver behavior. As built (ADR-1671 (3)): `note_injected_destroy`
  is not added, because `counts_as_loss` already counts `Destroyed` as a loss while the app runs (only
  `note_exiting()` makes a destroy benign); `mode: "destroy"` calls `Device::destroy()` on the `RenderDevice`.
- A real GPU hang would need a non-terminating shader; it is not provided. Manual hardware tests use the
  driver's own reset tools and are not automated.

## 6. Editor surface

Diagnostics panel, GPU tab: evidence level, counters (submitted / completed / in flight), the last frame's pass
list, and a "load last crash bundle" button. A timeline reader of `.regiatrace` (marks as vertical lines,
ranges as bars per lane, screenshot thumbnails at their frame) and a record button; both call the same
functions as the agent methods. The full profiler view is a later spec.

Status (ADR-2771): built. View ▸ Diagnostics (`regia_editor::diagnostics_panel`, `trace_timeline`); the window calls the
registered `render.breadcrumbs` and `telemetry.trace` handlers. The timeline places items on a time axis (zoom and pan),
ordered by frame; a screenshot thumbnail sits at its mark's time on a `screenshots` lane. Agent twins:
`editor.diagnostics`, `editor.trace_timeline`, `editor.trace_record` (`book/agents/editor-diagnostics.md`).

## 7. Determinism

- Marker order within a lane and pass order within a frame are deterministic for a fixed step and a fixed scene;
  frame numbers are the app frame counter.
- Times (`t_ns`, GPU intervals) are clock based and never compared in tests; tests compare names, order,
  depths, counts and frame numbers. Rate limits and windows are tested with an injected clock.
- Ring contents after N frames with a fixed scene are reproducible (names, markers, submission counts); tests
  read `completed` only after `device.poll(Wait)` (headless) or through the scripted source.
- Enabling breadcrumbs or the recorder never changes rendered output (test 9).

## 8. Tests and acceptance

"GPU" marks tests that need a device (lavapipe counts); all others run headless with `ScriptedProgress`. Temp
files use `regia_test_utils::unique_temp_dir`. No golden images.

1. Ring basics: 3 frames of 10 passes; `snapshot()` returns them newest first with names, open/close pairing,
   frame numbers and counts.
2. Wrap: `ring_frames + 3` frames keep only the newest `ring_frames`.
3. Overflow: more than `MAX_MARKERS` markers sets `truncated`, keeps the oldest, never panics; the next frame
   starts clean.
4. Submission order: two segments recorded on two workers in reverse CPU order and submitted as `[a, b]`
   appear as `a` then `b`; graph node ids interned at compile resolve to names.
5. Counters: submit 5, complete 1, 2, 4 (3 late): `completed == 2` until 3 arrives, then 4; duplicates ignored.
6. Evidence: a stall at submission S then a loss gives level `fenced` and suspects exactly the passes of
   submissions S..T in order; canned timestamps ending at P give `timestamped` with the pass after P first; the
   writer off gives `none` and the baseline bundle.
7. Bundle: a scripted loss runs the hooks; the bundle in a temp feedback folder holds `gpu_breadcrumbs.json`
   (parses, `format_version == 1`, adapter fields set, `lost.reason` set), the `crash.txt` block and the
   `feedback.json` part flag; exit code stays 3; with 64 frames x 150 passes the entry is <= 64 KiB and
   `truncated`.
8. Little information: no timestamps and an empty loss message give level 2 or 1, `message: ""`, no panic.
9. Output invariance (GPU): a fixed-step scene on lavapipe for 10 frames with breadcrumbs and recording on,
   then off: readback bytes identical (two runs compared, no golden).
10. Marks: 3 marks keep order with non-decreasing `t_ns`; the 101st per second for one name is dropped and
    counted (injected clock); each mark also emits `regia::mark`.
11. Ranges: A{B{C}} gives depths 0, 1, 2 and parents; closing A first force-closes C and B; an unknown id is
    counted; the 33rd level is dropped; stop closes open ranges.
12. Lanes: ranges on two threads nest independently.
13. File: write every event kind, read back equal; truncate at seeded offsets and read all complete chunks without
    error; unknown chunk kinds and JSON keys are skipped; the v1 fixture opens; a synthetic v2 header is
    refused with a clear error until a v2 decoder exists.
14. Export: JSON lines keep count and names; Trace Event output parses, has matched `B`/`E` pairs, instants for
    marks and `X` events for GPU intervals.
15. Window: `Windowed` then `start` gives a file whose first event precedes the call (injected clock);
    `save_window` writes a second file and leaves the running file unchanged.
16. Screenshot mark (GPU for pixels; path and frame logic headless): the PNG sits next to the trace, the frame
    number matches `play.step`, the export carries the relative path.
17. Coverage of pass sites (GPU): one frame of a sample scene records the shadow, opaque, post and UI passes by
    name.
18. Agent: every method's success and every reason; schema rows; book page present; `cargo xtask
    agent-coverage` and `check-layers` pass.
19. Off cost: with breadcrumbs disabled, a frame performs no allocation and no atomic store on the marker path
    (counting allocator and a store counter in the scripted source); a mark with no stream is one load.
20. Destroy injection (GPU, `test-hooks`): the process takes the loss path, writes the bundle with level >=
    `recorded`, exits with 3, and the bundle names the armed frame.
21. Budget as counts: at most 2 relaxed stores and one 8-byte list write per marker, no heap allocation per
    marker after warm-up.

## 9. Platform notes

- **Linux, Windows.** Levels 1-2 need core wgpu only; level 3 needs in-encoder timestamps (common on desktop, not
  guaranteed). Loss messages differ by backend and driver; stored verbatim, never parsed. A Windows driver
  timeout recovery can lose the device in every process at once; the report's unix time helps correlate.
- **Android aarch64.** Expect level 2 (in-encoder timestamps are rare). Surface loss on pause is not a device
  loss; the ring survives `suspended`/`resumed`. Completion callbacks for work in flight at suspension may never
  arrive: on resume the counters treat the first completed submission as a barrier (`completed = submitted`
  once it completes), so a later stall is not misattributed. The renderer's lifecycle handler calls
  `Breadcrumbs::note_resumed` on every `Resumed` (ADR-5640; before it nothing called it). Trace files go to the external files directory;
  agents reach the device through `adb forward`. Native crash capture is a no-op on Android today; nothing here
  depends on it.
- **macOS** (testing only): same core path; do not gate on it.
- **Native crashes** (Linux, Windows): the ring is plain data in static memory and is therefore inside the
  minidump; a reader in `cargo xtask minidump` is a follow-up (§ 11). No signal handler touches the ring.
- No new dependency (`lz4_flex` and `png` are already in the workspace; CRC is hand-written).

## 10. Budgets

Min-spec class (the min-spec Windows laptop laptop, Mobile Low), per frame, on top of the baseline:

| Item | On | Off |
|---|---|---|
| Pass markers (150 passes x 2) | < 0.02 ms, about 300 list writes, no allocation | one relaxed load per pass |
| Submission counters + callbacks (<= 4 submits) | < 0.01 ms | none |
| Timestamp backing (level 3) | < 0.02 ms CPU; 2 GPU writes per pass | none |
| Mark / range open+close | < 2 us each while writing | one load |
| `passes` stream to file | < 0.02 ms per frame (batched; chunk writes on a worker thread) | none |
| **Breadcrumbs + `passes` total** | **< 0.05 ms** | **no allocation or store** |

Memory: ring 8 x about 4 KiB; pass-name table <= 32 KiB; window 2 MiB by default (0 turns it off); two 256 KiB
write buffers; file IO on a worker thread, never the frame thread; bundle entry <= 64 KiB. Tracing spans:
`breadcrumbs_publish`, `breadcrumbs_resolve`, `trace_flush`, `trace_export`.

Decision (ADR-1585): breadcrumbs default on in every profile, including shipping and Mobile Low (field data is
the point); the lead measures the cost on the N150 and on an Android device before the v0.2.x release, and if
the total exceeds 0.05 ms the Mobile Low default becomes off. `BreadcrumbConfig.enabled` and `TraceConfig` are
settings, and the play-class methods change them in dev builds.

## 11. Out of scope and follow-ups

- Vendor post-mortem SDKs and GPU dump files (a separate ADR if wanted; licences differ per vendor).
- Fault-address resolution against live and recently freed allocations (needs a resource-lifetime tracker).
- The editor profiler UI, a trace server and live streaming; memory and network streams; typed custom events.
- Async compute lane beyond the name reservation; automatic bisection of suspects; a minidump ring reader.

## 12. Provenance

Algorithm sources (public sources only; the shared row is `docs/provenance.md`, "GPU breadcrumbs and trace markers spec"):

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Breadcrumbs | Markers around command-stream work; the last completed marker after a device loss bounds the fault | Vulkan specification, device-loss and queue-submission chapters (registry.khronos.org/vulkan); no vendor extension assumed |
| Submission tracking | Completion callbacks per submission to bound in-flight work | WebGPU specification, `GPUQueue.onSubmittedWorkDone` (w3.org/TR/webgpu); wgpu `Queue::on_submitted_work_done` (docs.rs/wgpu) |
| Timestamp backing | Timestamp queries and CPU/GPU clock correlation | WebGPU "timestamp-query" feature; Vulkan timestamp queries and `timestampPeriod` |
| Trace file | Length-prefixed, CRC-checked, append-only chunks; LZ4 block payloads | PNG chunk layout (RFC 2083) as the chunk + CRC model; LZ4 block format (github.com/lz4/lz4, `doc/lz4_Block_format.md`) |
| Export | `traceEvents` JSON with `B`/`E`/`i`/`X` phases | Trace Event Format (public document by the Chromium project, docs.google.com) |

### Owner decisions

None.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <https:<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 13. Implementation notes (ADR-2307)

- **Pass sites.** Every GPU pass of `render_into` and the modules it calls opens `breadcrumbs::frame_pass(name)`
  next to its `info_span!`, with the span's name; the outdoor composite's stamp callback brackets `atmosphere`,
  `clouds` and `fog`; each `WindowOverlay` layer opens one with the layer name (`ui`). `frame_pass` caches tags
  per thread, so a steady frame takes no name-table lock.
- **Timestamp backing (§ 2.3).** `TimestampMode::WhenSupported` turns on the frame's `GpuTimings` query set;
  a slot whose end stamp resolved calls `Breadcrumbs::note_resolved_frame`, so the evidence is "every pass of
  frame F finished". Per-pass stamps inside the frame are not added; the loss frame is bracketed by level 2.
- **Writer thread (§ 10).** `.regiatrace` chunks go to a writer thread over a bounded queue of 8 batches; a full
  queue never blocks the frame, and more than 4 MiB held stops the recording at its last complete chunk.
- **Frame observer.** The recorder follows `breadcrumbs::begin_frame` through `set_frame_observer`, which feeds
  the `frames` and `passes` streams; no app system is added.
- **Crash reports (§ 3).** `regia_platform::crash` writes a `crash-*.txt` for a device or surface loss and adds
  registered sections (the renderer's `gpu breadcrumbs:` block) to every report. `regia_crash` uploads the
  scrubbed UTF-8 text, not the raw file bytes (`scrub_upload`: invalid UTF-8 becomes U+FFFD, the home
  directory becomes `~`, secret values become `[redacted]`, email addresses become `[email]`). A breadcrumb
  section is still in the body; paths, secrets, and emails inside it are rewritten on the way out. A report
  that was already scrubbed when it was written is unchanged only where those patterns are already gone.
- **Trace header adapter (§ 4.2, ADR-5640).** The telemetry plugin sets the process recorder's adapter
  (`trace::adapter_label`: `name (backend, driver driver_info)`) when it installs the device hooks; before, the
  header's `adapter` was always empty.
- **Not built yet (ADR-5640, v0.5.x gaps).** `RenderGraph::execute` opens no breadcrumb scope per node and graph
  compile interns no node ids (the frame path does not run through `RenderGraph`, so plugin graph nodes appear
  only under their caller's scope); there is no `TracePlugin` and no `--record-trace` launch argument (hosts
  call `trace::configure` / `recorder().start`); the default trace folder is the relative `profiling` directory
  on every platform (the Android external files directory is the host's to configure); `gpu-breadcrumbs` and
  `regiatrace` are not registered with `cargo xtask check-formats` (§ 4.4); test 20 (destroy injection end to
  end) has no automated test (test 9 with a running recording is `regia_telemetry/tests/trace_output_invariance.rs`);
  tests 19 and 21 count allocations and list writes but not atomic stores.
- **Agent surface (§ 5).** `AgentPlugin` installs every method but `render.fault_inject`;
  `telemetry.screenshot_mark` is a deferred method; `render.fault_inject` takes `pass` (scripted loss inside a
  named pass, `unknown_pass` when it has not run).
- **As built (ADR-9070).** `RenderGraph::execute` interns its node ids the first time it runs with breadcrumbs on
  and opens one `breadcrumbs::frame_pass_tag` scope per live node in compiled order (a failing node's attempt
  included, a culled node none); the frame graph interns its registered passes when it is built and each runs inside
  its own scope. A host that submits a graph's encoder itself calls `breadcrumbs::note_untracked_submit`.
  `BreadcrumbsPlugin` and `TracePlugin` are in `regia::DefaultPlugins` (client). `TracePlugin` starts a recording at
  launch for `--record-trace[=<name>]`, `REGIA_RECORD_TRACE`, or on Android a `record-trace` file in the internal
  files directory, moves the trace folder to `<external files dir>/profiling` on Android
  (`trace::default_trace_dir`), and stops a running recording in the clean exit.
