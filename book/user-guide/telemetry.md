# Telemetry

Regia Engine emits structured telemetry as `tracing` events. The events cover a session header,
frame summaries, hitches, GPU errors, render problems and streaming problems. Each one has a
stable target and stable fields. The engine never writes a log file itself; the game's
`tracing` subscriber decides where the events go. `regia_telemetry::json_layer` is a
ready-made sink that writes one JSON object per line.

`TelemetryPlugin` is part of `regia::DefaultPlugins`. A game built on `DefaultPlugins` gets it
with no setup.

## Cost and safety

- **No per-frame log lines or allocations.** Each frame is added to buffers allocated once:
  three fixed-bin histograms, per-pass sums, a 63-frame median ring and fixed-size label rings.
  `tests/alloc_cost.rs` counts allocations over 1000 frames and asserts zero.
- **Per-system timing** (`top_systems`, v0.1.4) comes from `bevy_ecs`'s `trace` feature
  (one span per system run) read by the frame-profiler layer; see `tests/span_cost.rs` for its
  cost per frame. The phase split is two `Instant::now()` reads per schedule.
- **≤ 0.05 ms CPU per frame.** The same test times the per-frame system against that bound.
  On a real scene (`do27_bench`, Switchbacks, High, 2560×1440, RTX 5080, `REGIA_TELEMETRY=verbose`):
  - telemetry's own frame work: 2.3–3.3 µs mean, p95 ≤ 4.8 µs;
  - the optional frame-profiler layer: +26 ns per span, at about 58 spans per frame, so about
    1.5 µs per frame;
  - a frame that detects a hitch (span capture): 20–24 µs; the frame that emits the hitch
    event: 22–24 µs;
  - GPU frame p50/p95: unchanged within 1 %.
- **Slow probes run off the frame thread.** RSS (`/proc`) and VRAM (wgpu's allocator report)
  are sampled by a background thread every 10 s. The frame only reads atomics.
- **Rate limits.**
  - The same event key within 5 s is emitted once. The next event after the window carries
    `repeated: N`.
  - Hitch events use a token bucket: 5 at once, then one every 2 s. Suppressed hitches are
    counted in the next summary.
  - Budget-exceeded and eviction-storm events are emitted at most once a minute.
- **Never blocking.** Events are only events. `json_layer` hands each line to a bounded queue
  with `try_send`. A full queue drops the line and counts it.
- **GPU errors do not crash a release build.** wgpu's default error handler panics. Telemetry
  installs a handler that logs the error, and panics afterwards only in debug builds
  (`TelemetryOptions::gpu_error_panics`). See ADR-159.

## Levels and switches

| Setting | Effect |
|---|---|
| `TelemetryOptions::level = Verbose` (default) | Everything below. |
| `TelemetryOptions::level = Normal` | The session header, summaries and warnings/errors only. Hitches under 250 ms and boot events are not emitted. |
| `TelemetryLevel::Ship` (v0.5.1) | What `Normal` emits, safe to upload from a player build: see [Ship level](#ship-level). |
| `REGIA_TELEMETRY=off\|normal\|verbose\|ship` | Overrides `enabled` and `level` at startup. With `ship`, the engine's own JSON log (`REGIA_JSON_LOG`) is filtered at `Ship` too. |
| `JsonLayerOptions::level` | The same `Normal`/`Verbose`/`Ship` filter, applied by the JSON writer to every event it receives (the game's own included). |

To change the options, add `TelemetryPlugin { options }` before `DefaultPlugins`, or replace
the `Telemetry` resource with `Telemetry::new(options)`.

## Ship level

`TelemetryLevel::Ship` (v0.5.1, ADR-8417) is the level a game ships to players whose logs it
uploads. It emits what `Normal` does, with the machine-identifying parts left out. The field list
below is the contract: a game can show it to its players as "what this upload contains".

**Session header (`regia::session`).** Built from an allowlist (`SHIP_HEADER_KEYS`). A key added
to the full header in a later release is not written at `Ship` until it is added to that list.

| Field | Meaning |
|---|---|
| `regia.version`, `regia.profile` | Engine version, `debug`/`release`. |
| `os.family`, `os.arch` | `linux`, `windows`, `android`, `macos`; `x86_64`, `aarch64`. No OS version or build. |
| `ram_gb` | RAM tier: 4, 8, 12, 16, 24 or 32 (`SHIP_RAM_TIERS_GB`). A machine reports the smallest tier at or above its usable RAM, which reads a little under the installed size. 32 means over 24 GB. Null when unknown. |
| `vram_gb` | The adapter's device-local memory tier: 2, 4, 6, 8, 12, 16, 24 or 32 (`SHIP_VRAM_TIERS_GB`), read the same way. 32 means over 24 GB. Null when unknown. |
| `adapter.class` | `discrete`, `integrated`, `virtual`, `cpu` or `other`. |
| `adapter.vendor` | Vendor family: `nvidia`, `amd`, `intel`, `arm`, `qualcomm`, `apple`, `imagination`, `broadcom`, `mesa` (software), `other` or `unknown`. |
| `adapter.backend` | `Vulkan`, `Dx12`, `Metal`, `Gl`. |
| `preset` | The `GameSettings` graphics preset name, null without `GameSettings`. |
| `render_scale` | The `GameSettings` render scale, null when automatic or unset. |
| `telemetry_level` | `ship`. |

Not written at `Ship`: the OS version, CPU model and thread count, adapter name, PCI vendor and
device ids, driver, features, limits, fallbacks, monitors, window, `settings_ron`, `host`,
`boot_marks`, `pipeline_cache` and `problems`. The event's message names the adapter as
`<vendor> <class>` (`regia session: nvidia discrete (Vulkan)`).

**Events.**

- `regia::frame_summary` is unchanged: frame-time p50/p95/p99, max and mean, CPU and GPU time,
  `bound` and `frame_bound`, `gpu_pass_ms`, `render_scale_permille`, the wait times, draw and
  memory counts, and `hitches` / `hitches_suppressed`. It holds numbers and engine system and
  span names, nothing from the machine.
- `regia::hitch` is never written, even for a frame of 250 ms or more. Hitches are counted in
  the summary's `hitches` and `hitches_suppressed`, and in `telemetry.get_last_summary` counts.
  A hitch record names resources and host spans.
- `regia::boot` is not written (as at `Normal`).
- `regia::problems` carries `[{code}]`, without the message text.
- Warnings and errors (`regia::gpu_error`, `regia::surface`, `regia::stream` and the game's own)
  are written as at `Normal`. Their text has the home folder scrubbed (see below). It can still
  name assets by their project path.

**`summary.json`.** `system` and `display` come from the Ship header: `system.os` is
`<family> <arch>`, `ram_gb` and `vram_gb` are tiers, `vendor` is the family. `cpu`,
`cpu_threads`, `gpu` and `driver` are null, and `display` has no monitors or window size. There is
no `host` section, and `settings.auto_plan` (which names the adapter) is null. A game that wants
its own version in the upload adds it itself.

A JSON layer at `Ship` (`JsonLayerOptions::level`) keeps what `Normal` keeps, but never writes
the `regia::boot` and `regia::hitch` targets. Set both `TelemetryOptions::level` and
`JsonLayerOptions::level` to `Ship`, or set `REGIA_TELEMETRY=ship`, which sets both for
`DefaultPlugins` and the engine's `REGIA_JSON_LOG` file.

## Wiring a JSONL file (one line)

```rust,ignore
use tracing_subscriber::prelude::*;

let (json, _json_guard) = regia::telemetry::json_layer(logs_dir.join("game.jsonl"), Default::default())?;
tracing_subscriber::registry()
    .with(human_log_layer) // the game's own text log, with its own filter
    .with(json.with_filter(EnvFilter::new(regia::telemetry::RECOMMENDED_FILTER)))
    .with(regia::frame_profiler()) // optional: hitch events then name their top CPU spans
    .init();
// Keep `_json_guard` alive until exit: dropping it flushes the queue (waits at most 2 s).
```

**Always give the JSON layer a filter.** `RECOMMENDED_FILTER` is
`info,regia=debug,regia_render=debug,regia_terrain=debug,wgpu_core=warn,wgpu_hal=warn,naga=warn,fontdb=error`.
Without one, naga's shader-compiler `log` output reaches the layer: about 140 000 lines per level
load on `do27_bench`. `fontdb=error` (v0.5.1, also in `regia_core::log::DEFAULT_FILTER`) drops the
font database's per-file "malformed font" warnings, which name a player's font paths: the engine
counts the system font files it could not parse into one `debug` line per launch instead
(`regia_ui::text::count_font_scan_warnings`; the line names the count, never a path). Formatting those cost about 1 ms of `render_into` CPU per frame and filled
the 64 MB cap. The layer also drops `TRACE` events before formatting them (`max_level`), and past
the cap it formats only warnings and errors.

`json_layer(path, opts)` creates or truncates `path`. `JsonLayerOptions`:

| Field | Default | Meaning |
|---|---|---|
| `max_bytes` | 64 MiB | After this many bytes only warnings and errors are written, after a `"log size cap reached"` line. Writing stops at 2 × `max_bytes`. |
| `queue_lines` | 4096 | Queue length. When it is full, lines are dropped and counted. A `"dropped lines (queue full)"` line with the running total is written at most once a second. |
| `level` | `Verbose` | `Normal` keeps warnings, errors and `always_targets`. `Ship` keeps the same, minus `regia::boot` and `regia::hitch`. |
| `max_level` | `DEBUG` | More verbose events are dropped before they are formatted. |
| `always_targets` | `regia::session`, `regia::frame_summary`, `regia::boot`, `regia::phase`, `regia::exit` | Written at `Normal` whatever their level. |
| `flush_interval` | 1 s | Buffered lines are flushed at most this often. A warning or error triggers an immediate flush plus `fsync`, with at most one `fsync` per interval. |
| `repeat_window` | 5 s | The same target and message are written once per window. The next line after the window carries `"repeated": N`. |

If writing fails (disk full, permissions), the sink turns itself off and says so once on
stderr. It never panics.

`JsonLayerGuard::stats()` returns `{dropped, written_lines, written_bytes, capped, failed}`.
Put `dropped` in your own summary line.

Each line looks like this:

```json
{"t_ms":10012.4,"wall":"2026-10-09T18:02:11.532Z","level":"INFO","target":"regia::frame_summary","msg":"frame summary (periodic): 1187 frames, p50 8.25 ms, p99 12.5 ms, 0 hitches","fields":{"reason":"periodic","frames":1187,"p50_ms":8.25,"p99_ms":12.5,"hitches":0,"data":{…}},"span":[]}
```

- `t_ms` is milliseconds since the layer was created.
- `wall` is UTC wall-clock time.
- `fields` holds the event's fields. On a `regia::*` event, `data` is the full record as a
  JSON object.
- `span` lists the enclosing span names, outermost first.
- **`fields.data` is a nested JSON object** (v0.1.9, spec 28), for host events as well as
  `regia::*` ones. The same text is also written (except on `frame_summary` and `hitch`) as the string `fields.data_json` for this one
  release, for parsers that still read the old shape; `data_json` is removed in a later breaking release. A `data`
  value that is not a JSON object or array stays a string. Parsers should accept both.
- **The user's home folder never reaches the log.** Engine messages and string fields have
  `C:\Users\<name>\…` / `/home/<name>/…` written as `~\…` / `~/…`
  (`regia_core::privacy::scrub_home`, which a host can call on its own messages).

## Events

Every event has a short human `message`, a few flat fields for grep, and `data`, which holds the
full record as JSON. The tables below describe `data`.

### `regia::session` (INFO, once)

Emitted at the end of the first frame that rendered with a render device. A headless app gets
it after 300 frames.

| Field | Meaning |
|---|---|
| `regia.version`, `regia.git`, `regia.profile` | Crate version, `REGIA_GIT_HASH` at build time, `debug`/`release`. |
| `os.family`, `os.arch`, `cpu_threads` | Platform. |
| `os_version`, `cpu`, `ram_gb`, `vram_gb` | OS version string, processor model, physical RAM, and the adapter's device-local memory (null when unknown). Read once at startup, never on the frame thread. |
| `adapter` | `name`, `vendor` / `device` (hex PCI ids), `device_type`, `driver`, `driver_info`, `backend`. |
| `features` | The device's enabled wgpu features (the requested set). |
| `limits` | The main device limits: texture size and layers, bind groups, storage buffer binding size, buffer size, per-stage storage buffers and sampled textures, compute workgroup storage and invocations, color attachments. |
| `fallbacks` | Active fallbacks: `software_adapter`, `adapter_selection: …`, `no_timestamp_query`, `no_multi_draw_indirect_count`, `no_bc_compression`, `no_pipeline_cache`. |
| `monitors` | `name`, `width`, `height`, `refresh_hz`, `scale`, `current` per monitor. |
| `window` | Primary window `physical_size`, `scale_factor`, `mode`, `present_mode`. |
| `settings_ron` | `GameSettings` as RON, when the resource exists. |
| `host` | `TelemetryHost` fields set by the game (version, content hashes, …). |
| `boot_marks` | `[{mark, ms}]` since process start (`render.boot_timeline`). |
| `pipeline_cache` | `enabled`, `load` (`hit (N bytes)` / `miss` / `discarded (reason)`), `load_ms`, `pipelines_created`. |
| `problems` | `render.problems` at that frame: `[{code, message}]`. |
| `telemetry_level` | `normal` or `verbose` (at `ship`, see [Ship level](#ship-level)). |

### `regia::phase` (INFO)

One event per transition of the session (spec 28 § 3). `data` is `{phase, …}`; the engine and the
host share the target so one tool reads both.

| Phase | Emitted by | Other `data` fields |
|---|---|---|
| `boot_done` | engine | `ms` since process start, at the first presented frame. |
| `shaders_ready` | engine | The startup pipeline warm: `pipelines`, `ms`, `ms_each`, `cache` (`hit`/`miss`/…), `threads`, `backend`, `boot_tier_ms`. |
| `menu_ready` | host | |
| `scene_load_start` / `scene_load_end` | engine (the scene warm, ADR-190) and host | End: `ms`, `mb_uploaded`, `pipelines`, `meshes` (and `timed_out`, `error`). The host's carry `name` (the map). |
| `match_start` / `match_end` | host | `name` (the map); end: `result` fields at the host's choice. |
| `quit_start` / `quit_done` | engine (the clean exit, ADR-188) | Done: `ms` and `steps: [{step, ms}]`. |
| `previous_session_unclean` | engine (WARN) | The previous session left a `summary.partial.json` and no `summary.json`. |

A host emits its phases with `regia_telemetry::phase("match_start", json!({"name": "bore_twelve"}))`.
A `name` on `scene_load_*` or `match_start` makes the frames after it count for that map in
`summary.json`'s `frames_by_map`; `match_end` and `menu_ready` end the map.

### `regia::exit` (INFO, once)

Emitted at the end of the clean exit (ADR-188): `total_ms`, `left_fullscreen`, `entities_dropped`
and `steps: [{step, ms, budget_ms, over_budget}]` in sequence order (`stop_background_work`,
`leave_fullscreen`, `wait_gpu_idle`, `drop_surfaces`, `drop_gpu_resources`, `drop_device`,
`save_caches`).

### `regia::boot` (INFO, Verbose)

Emitted when a boot mark is recorded after the header, for example `game_warm_start`,
`first_game_frame` or `splash_done`. `data`: `frame`, `boot_marks` (the whole timeline). The flat
field `mark` holds the newest mark.

### `regia::frame_summary` (INFO)

Emitted every `summary_interval` (10 s), and at the end of the frame in which the game calls
`Telemetry::request_summary(reason)` or `regia_telemetry::request_summary(world, reason)`.
Call it at level end, match end and return to menu. Each summary starts a new window.
`regia_telemetry::last_summary_json()` returns the newest summary from any thread, for crash
and stall reports.

| Field | Meaning |
|---|---|
| `seq`, `reason`, `frame`, `t_s`, `window_s` | Summary number, `periodic` or the requested reason, the app frame, the real time, the window length. |
| `frames`, `gaps` | Frames in the window. Gaps are intervals over 5 s (suspend, debugger) and are not counted as frames. |
| `frame_ms` | `p50`, `p95`, `p99`, `max`, `mean` of the frame interval. Resolution is 1/8 ms up to 64 ms and 4 ms up to 1 s. |
| `fps_histogram` | Frames per bucket: `ge_240`, `ge_144`, `ge_120`, `ge_90`, `ge_60`, `ge_45`, `ge_30`, `ge_20`, `ge_10`, `lt_10`. |
| `cpu_ms` | `mean`, `p95`, `max` of the renderer's CPU time: `FrameRenderer::render_into` only (cull, record, submit). The sim, extract, the swapchain acquire and the present are not in it; `frame_breakdown_ms` has the whole main thread. |
| `gpu_ms` | `mean`, `p95`, `max` of the GPU frame (0 without timestamp queries). |
| `bound` | `gpu` or `cpu`, whichever mean is larger (renderer CPU against GPU, not against the frame). `unknown` without GPU timing. Kept as is; `frame_bound` is the class to act on. |
| `bound_reason` | v0.5.1. Why `bound` is `unknown` (null otherwise): the most frequent unknown reason in the window. `no_gpu_timing` (no renderer, or GPU timing off or unsupported), `loading` (the newest phase marker is `scene_load_start`, `loading`, `loading_*` or ends in `_load_start`), `no_scene` (the window drew UI or a splash only, or a game-frame warm has the renderer), `no_gpu_sample` (timing on and a scene drew, but no sample was read back yet), `hidden`, `renderer_idle`, or `event_wait`. `frame_bound.unknown_reasons` counts each. |
| `frame_bound` | Timing-share classification: `gpu_bound` at 90 % or more, `cpu_bound` at 75 % or less, and `balanced` between. v0.5.1 requires fresh CPU/GPU publications, a visible runner, and event sleep below half the interval; otherwise `unknown`. The summary `class` and `gpu_share` use eligible pairs only (null share without any). `frames: {cpu_bound, gpu_bound, balanced, unknown}` counts ticks; `counted` is their sum. `unknown_reasons` counts `no_gpu_timing`, `loading`, `no_scene`, `no_gpu_sample`, `hidden`, `renderer_idle`, and `event_wait`. This is not CPU utilization; see the freshness section below. |
| `render_scale_permille` | v0.1.14. The render scale the renderer drew at, internal width × 1000 / output width: `last` (the newest frame), `mean`, `min`, `max` over the window. What was drawn, not the setting (dynamic resolution shows here); never persisted. Null before the renderer drew. |
| `wait_acquire_ms`, `wait_present_ms`, `wait_readback_ms` | v0.1.14. `{mean, max}` per window frame of the main thread's waits on the GPU: the swapchain acquire (`get_current_texture`), `Queue::present`, and blocking screenshot readbacks (0 in frames without a screenshot). From `regia_render::frame_waits`; null when no window presented. |
| `cpu_wait_on_gpu_ms` | v0.1.14. `{mean, max}` of the three waits summed per frame. The rest of the frame's wait (the compositor's redraw gate, inside `outside_update`) is not split yet. |
| `live` | The newest telemetry tick (`Telemetry::live`, `telemetry.live`): `frame`, `frame_ms`, numeric `cpu_ms`/`gpu_ms`, `bound`, `bound_reason`, `render_scale_permille`, `wait_acquire_ms`, `wait_present_ms`, `wait_readback_ms`. v0.5.1 adds optional `timing`, nullable `cpu_sample_ms`/`gpu_sample_ms`, `runner`, `event_wait_ms`, and `phases_frame`; see the freshness section below. |
| `gpu_pass_ms` | `{mean, max}` per pass: `shadows`, `clusters`, `gi_probes`, `prepass`, `cull`, `ssao`, `contact_shadows`, `forward`, `terrain_forward`, `forward_glass` (v0.1.17: the forward pass's translucent section, every glass pane with the own canopy and the other blended draws; 0 without in-pass timestamps), `ssgi`, `ssr`, `atmosphere`, `clouds`, `fog`, `upscale`, `post` (translucency, VFX and the post stack), `scene`, `frame`. |
| `probe_update_ms` | `{mean, max}` of the GI probe update (`gi_probes`). |
| `draws` | `drawn_mean`, `drawn_max`, `instanced_mean`, `per_object_mean`, `culled_mean`, `skipped` (window total of draws skipped because a mesh, object or material entry was gone; 0 in a healthy run, see [draw integrity](../agents/draw-integrity.md)), `skinned_culled_mean` (skinned meshes culled by their joint-palette bound), `zero_key_moving_meshes` (window maximum of moving meshes with `object_key 0`; should be 0). |
| `emissive` | v0.1.18. The emissive fit cache over the window (area lights fitted to emissive meshes, see [Emissive lights](../agents/emissive-lights.md)): `fits_done` (fits computed), `fits_deferred` (fits sent to the background worker), `cache_hits` (new mesh keys whose fit came from the content cache), `probe_hits` (new mesh keys that took the probes of the same object's earlier mesh of the same content), `probes_traced`, `budget_holds` (mesh-frames whose new work waited for the 1 ms frame budget), `stand_ins` (mesh-frames drawn with the lights of the mesh they replaced), `frame_ms` (frame-thread time in the collection, summed), `frame_ms_max` (the window's costliest collection), `worker_ms`, `pending` (fits and probe batches in flight now), `background` (the worker is in use). A `frame_ms_max` over a few ms with `fits_deferred` 0 means the work ran on the frame thread (the load frame, or `REGIA_EMISSIVE_SYNC=1`). Null without a renderer. |
| `scatter_emitted` | Scatter instances handed to the last frame. |
| `particles_live` | Estimated live particles after the last VFX update. |
| `textures` | `resident_bytes`, `budget_bytes` (null when unlimited), `over_budget`, `streamed_materials`, `queue_depth` (sets being filled), `fading`, `levels_below_cap`, `cpu_bytes`. |
| `memory` | `rss_bytes`, `rss_peak_bytes` (Linux/Android), `vram_allocated_bytes`, `vram_reserved_bytes` (wgpu allocator report). Values are up to 10 s old; null when unknown. `subsystems` (v0.1.28): `heap_tracked`, `heap_live_bytes`, `heap_peak_bytes` and `by_subsystem: { <name>: { heap_bytes, gpu_bytes } }`, CPU heap bytes per engine subsystem (null unless the host installed `regia_core::memory::TaggingAllocator`) and GPU bytes from the last probe sample, see [Memory per subsystem](../agents/memory.md). `host` (v0.5.1, ADR-7891): `cpu_mesh_bytes`, `cpu_texture_bytes`, `dropped_bytes`, `reread_bytes`, `rereads`, `over_budget_ticks` of the engine's CPU mesh and texture copies under the host memory budget, see [Load memory](../agents/load-memory.md). |
| `hitches`, `hitches_suppressed` | Hitches in the window, and how many were not emitted because of the rate limit or level. |
| `present_waits` | v0.5.1. Frames in the window that passed the hitch tests but spent most of their excess over the median waiting on the swapchain or display (see below). Not in `hitches`. |
| `created`, `created_after_warmup` | Frame-path `pipelines`, `bind_groups`, `buffers`, `textures`, `blocking_polls`, `inline_bakes`, in the whole window and after warm-up. |
| `frame_scratch` | `growths` and `growths_after_warmup`: times the renderer's reusable per-frame lists had to grow in the window (0 in a settled scene); `total` since the renderer was created (v0.1.28, ADR-385; [Frame list growth](../agents/frame-scratch.md)). |
| `warm` | Warm-up is over: 120 frames after the `first_game_frame` boot mark, or 1800 frames if the game never records it. |
| `surface_errors` | `outdated`, `lost`, `timeout`, `other` in the window. |
| `gpu_errors`, `asset_loads`, `texture_evictions` | Counts in the window. |
| `notes_dropped`, `events_suppressed` | Stream notes lost to a full ring, and events held back by repeat limits (since start). |
| `frame_breakdown_ms` | Mean main-thread ms per frame by phase, from `regia_app::FramePhases` (two clock reads per schedule): `outside_update`, `host_schedules`, `pacing`, `first`, `pre_update`, `state_transition`, `fixed_update`, `update`, `post_update`, `extract`, `render`, `last`, `clear_trackers`, `event_wait`. Optional `runner` is the most recent completed phase record's visibility and throttle configuration. `render_split` is `{acquire, render_into, tail, present, other}` of `render` (`regia_render::window_phases`). `sum_ms`, `frame_ms` (the window mean) and `unaccounted_ms` = `frame_ms` − `sum_ms`; `fixed_steps_per_frame`; `frames`. Null without an app. |
| `top_systems` | `{timed, all_systems_ms, top: [{system, mean_ms, p95_ms, max_ms, runs_per_frame}]}`: the 10 costliest ECS systems by mean ms per window frame. Timed from `bevy_ecs`'s per-system spans by `regia::frame_profiler()`; `timed` is 0 when the subscriber lacks that layer. `p95_ms` is a bucket edge (within a factor √2). Systems on worker threads overlap, so `all_systems_ms` can exceed the frame. Exclusive systems and observers are not listed. |
| `churn` | `created_by_label: [{kind, label, count, bytes}]` (12 most frequent frame-path creations), `labels_not_kept`, `pipelines_by_label`, `upload_bytes: {total, per_frame, belt, texture, mesh, other_buffer}` (`other_buffer` = total − belt − texture − mesh, signed: negative means a source was counted twice), `mesh_buffers_created`, `cache: {meshes, materials, objects, *_growth}` (renderer caches now and their change over the window). |
| `host_spans` | The host's own sections, by span name: every span whose target starts with a prefix from `regia_core::profiling::set_host_span_prefixes`. Until that call succeeds, a span matches `do27::` (`DEFAULT_HOST_SPAN_PREFIXES`) or `default_host_span_prefix()` (the executable stem plus `::`, when the stem is usable: not empty, and not starting with `regia` or a reserved crate name such as `wgpu`). A successful call replaces both defaults; a later call returns false and changes nothing. `main: {sections: [{span, target, mean_ms, max_ms, calls_per_frame}], top_level_ms, frame_ms, unattributed_ms}` covers the thread that runs `App::update`. `top_level_ms` counts nested sections once, and `unattributed_ms` = `frame_ms` − `top_level_ms` is the main-thread time outside every host section (engine phases included). `other_threads: {sections}` covers spans on any other thread (a sim thread). Sections are inclusive and nest inside the `frame_breakdown_ms` phases. Needs `regia::frame_profiler()`; null when the host emits no matching span. |
| `counts` | `{mean, max}` of `mesh_instances` (extracted), `drawn`, `per_object_draws`, `local_lights`, `shadow_casters`, `culled`, `skinned_draws` (skinned colour draws) and `skinned_batches_possible` (the draws they would take batched by cull mode, mesh, material and pipeline; [skinned instancing](../agents/skinned-instancing.md)); `entities` (spawned) and `entity_slots` at the summary. |
| `texture_views` | Render-to-texture cameras (v0.1.6): `views_mean` and `views_max` (texture cameras extracted per frame), `renders_per_frame` (views rendered; the rest were paused or between `update_every` refreshes), `cpu_ms_per_frame` and `gpu_ms_per_frame` (all views, summed), `frames`, and `per_view: [{asset_id, renders, cpu_ms_mean, gpu_ms_mean}]` (up to 8 views). GPU ms needs GPU timing and lags two frames; 0 otherwise. All zero when the host has no texture camera. |

Not counted yet: triangles (the renderer does not count them). Physics, audio and network
counters belong in a game-side `do27::summary` line.

### `regia::hitch` (INFO; WARN at ≥ 250 ms)

A frame counts as a hitch when it is at least `hitch_min_ms` (8 ms) and stands out from the
rolling median of the last 63 frames:

- its interval exceeds `hitch_ratio` (2) × the median, or
- its interval exceeds the median by more than `hitch_abs_ms` (50 ms).

Both tests are relative (v0.1.14). Before, any frame over 50 ms was a hitch, so a game running a
steady 66 ms logged one every 2 s (the rate limit's pace). A steady slow frame rate is not a
hitch; a spike out of it is (66 ms steady, then 117 ms). A step down (16 ms to 66 ms) counts
until the median catches up, about 32 frames. Every interval over 5 s is a gap, as before.

Since v0.5.1 (ADR-7898) two more rules keep pacing out of the hitch count:

- **The pacing floor.** When a frame cap (`FramePacing::target_fps`) or vsync (the primary
  window's present mode, with the monitor's refresh rate from the session header) paces the loop,
  a frame shorter than `hitch_floor_periods` (1.5) × that period is never a hitch:
  the floor is `max(hitch_min_ms, 1.5 × period)` (`regia_telemetry::hitch_floor_ms`). Under 60 Hz
  vsync that is 25 ms: a frame that missed one vsync (33 ms) still counts. A harness that drives
  `Telemetry::observe_frame` sets the period with `Telemetry::set_pacing_period_ms`.
- **Swapchain waits.** A frame that passes the tests but spent at least `present_wait_share` (0.5)
  of its excess over the median in this frame's acquire + present wait, with an excess of at most
  `hitch_abs_ms`, is a `present_wait`: counted in `TelemetryCounts::present_waits` and the summary's
  `present_waits`, not a hitch, and no event. A longer wait is a stall, whatever it waits on, and
  stays a hitch. A DO27 tester session logged 6324 "hitches" that were 14 ms frames against a 7 ms
  median, the excess spent in acquire; replayed through v0.5.1 they are 0 hitches and 6324 present
  waits.

The event is emitted two frames later, because GPU timings are read back two frames late. When a second hitch arrives inside that wait, the first is emitted at once without `gpu_ms` and `top_gpu_passes` (the timings then on hand belong to an earlier frame).

| Field | Meaning |
|---|---|
| `frame`, `renderer_frame`, `t_s` | App frame, renderer frame index, real time. |
| `frame_ms`, `median_ms`, `ratio` | The interval, the median before it, their ratio. |
| `gap` | The interval was over 5 s (suspend or debugger, not a real hitch). |
| `cpu_ms`, `gpu_ms` | Fresh renderer CPU time (`render_into` only, as in the summary); optional GPU time whose source frame matches that fresh render. GPU time is omitted if the matching readback has not arrived. Optional `render_timing` and `gpu_frame_id` expose the source and freshness checks. |
| `frame_ms_wall`, `update_ms_wall` | v0.1.14. That frame's main-thread wall time from `FramePhases`: every phase (end of the update before to the end of this one), and `App::update` alone (without `outside_update`, `host_schedules`, `pacing`, and `event_wait`: the sim, extract and the render with its waits). Absent when the app does not publish `FramePhases`. |
| `bound`, `render_scale_permille` | v0.1.14. That frame's bound class and drawn render scale (see the summary's `frame_bound` and `render_scale_permille`). |
| `wait_ms` | v0.1.14. `{acquire, present, readback}`: the newest window frame's waits on the GPU, ms. |
| `top_cpu_spans` | `[{span, ms}]`: the 5 largest span totals of that frame on the frame threads (the main thread and the render thread; v0.5.1: before, any thread's) from the frame profiler. Spans are inclusive, so parent spans rank first. Empty unless the subscriber includes `regia::frame_profiler()`. |
| `background_spans` | v0.5.1. `[{span, ms}]`: the 5 largest span totals of that frame on every other thread (a warm worker, a bake, the asset loader). Background work, never the cause. |
| `cause` | v0.5.1. What the frame was doing (also counted in `summary.json`'s `hitches.top_causes`), first match wins: `outside_frame` when `unaccounted_ms` is at least half the frame; `pipeline_create`, `blocking_poll`, `inline_bake`, `gpu_resource_create` from the creation counters; the largest frame-thread span when it took at least half the excess over the median; else the largest measured phase of the interval (`event_loop`, `pacing`, `host_schedules`, `phase_<name>`); else `unknown`. `gap` for a gap. A worker's span is never the cause. |
| `unaccounted_ms`, `event_loop_ms`, `interval_ms` | v0.5.1. The frame interval is timed from `First` of the previous update to `First` of this one, so it holds the previous update's schedules after `First` and this update's time between updates. `interval_ms` is that split: `frame_ms`, `measured_ms` (the phases inside the interval), `unaccounted_ms` (`frame_ms` − `measured_ms`: time inside no phase the engine measures: the OS, a window-mode change outside the event loop, a driver call, the process not scheduled), `event_loop_ms` (the window event loop before this update), `host_schedules_ms`, `pacing_ms`, `first_ms`, `previous_update: {pre_update … clear_trackers}`, and `largest: {phase, update: previous\|this, ms}`. Null when the app does not publish `FramePhases`. `frame_breakdown_ms` stays this update's own phases. |
| `window_events` | v0.5.1. Window events of the frame and the one before (the interval spans both): `resized`, `scale_changes`, `focus_gained`, `focus_lost`, `mode_changes` (the primary window's `WindowMode` changed), `any`, and the window's `mode` (`windowed`, `borderless_fullscreen`), physical `size` and last known `focused`. |
| `hitch_floor_ms` | v0.5.1. The floor in effect (see above). |
| `bound_reason` | v0.5.1. Why `bound` is `unknown` (as in the summary), null otherwise. |
| `top_gpu_passes` | `[{pass, ms}]`: the 5 largest GPU passes. |
| `created` | `pipelines`, `bind_groups`, `buffers`, `textures`, `blocking_polls`, `inline_bakes` in that frame, plus `labels`: `[{kind, label, bytes}]`, up to 12, newest first. |
| `warm` | Warm-up was over. |
| `frame_breakdown_ms` | That frame's phases (as in the summary, without the means) and `render_split`, plus `sum_ms`. Absent when the app does not publish `FramePhases`. |
| `top_systems` | `[{system, ms, runs}]`: that frame's 5 costliest ECS systems (needs `regia::frame_profiler()`). |
| `host_spans` | `{top_level_ms, sections: [{span, ms}]}`: that frame's 8 largest main-thread host sections (null without any). |

### `regia::gpu_error` (ERROR)

From wgpu's uncaptured-error callback. The same kind, pass and message within 5 s are held back (a fixed 5 s window, not `repeat_window`; the message is compared by its first 200 bytes).

| Field | Meaning |
|---|---|
| `kind` | `validation`, `out_of_memory` or `internal`. |
| `frame` | The app frame. |
| `pass` | The innermost `tracing` span on the thread that made the failing call. This is usually the render pass span, such as `forward_pass`. |
| `message` | wgpu's description, cut at 2 KiB. |
| `repeated` | Errors held back since the last one. |

### `regia::device_lost` (ERROR; INFO when destroyed on purpose)

`reason` (`unknown` or `destroyed`), `frame`, `message`.

### `regia::surface` (WARN)

Emitted with a summary when the window had surface acquire errors. Fields: `outdated`, `lost`,
`timeout`, `other` (occluded, validation), the `total_*` since start, `frame`, `window_s`.

### `regia::problems` (WARN; INFO when cleared)

Emitted when `render.problems` changes, which includes the first non-empty list. `data`:
`frame`, `count`, `problems: [{code, message}]` (`[{code}]` at `Ship`). The flat field `codes` is
comma-separated.

### `regia::stream` (WARN)

`kind` says which:

| `kind` | When | Extra fields |
|---|---|---|
| `budget_exceeded` | Resident streamed textures over the budget at a summary (at most once a minute). | `textures` (as in the summary) |
| `eviction_storm` | `eviction_storm` (32) or more texture-set evictions per second, measured over the real window since the last check (at least 1 s; at most once a minute). | `evictions_per_s` |
| `wrong_platform_textures` | More textures took the wrong-platform fallback since the last summary. | `decoded`, `decoded_bytes`, `unbound`, `formats`, `labels` |
| `slow_asset_load` | A mesh, texture, image, material or model load took over 100 ms. | `label` (`kind id`), `ms` |
| `pak_corrupt` | A pak entry failed its integrity check (`PakError::Corrupt`, hash mismatch). | `label` (`pak entry id`), `offset` (-1 when unknown) |
| `pipeline_after_warmup` | A frame after warm-up created pipelines, i.e. a shader compile mid-game. | `pipelines`, `labels` |

Every `regia::stream` event also has `frame` and `repeated`.

## `summary.json`: one record per session

`TelemetryPlugin` also writes the session's numbers as one JSON file, so a tool can answer "what
happened on this PC" without reading the whole log (spec 28 § 4; schema
`regia_telemetry::SessionSummary`, `"schema": 1`).

| File | When |
|---|---|
| `<stem>.summary.partial.json` | 5 s after the session header, then every 60 s. Written on a background thread to a temporary file and renamed over the old one, so a reader never sees half a file. |
| `<stem>.summary.json` | At the clean exit, inside the exit sequence before the world drops (ADR-188), and again once the sequence is over with `exit.ms` and the step timings. On a caught panic, from the newest partial with `exit.panic`. |

`<stem>` is the JSON log's file name without `.jsonl` (the `json_layer` path). Set
`TelemetryOptions::summary_path` to choose the final file's path (the partial goes beside it,
`summary.json` → `summary.partial.json`); `TelemetryOptions::summary = false` turns the files off. A
process with no JSON log and no `summary_path` writes none.

A session that hard-locks never reaches the clean exit, so the next session looks at the newest
earlier session in the folder (or, for a folder-per-session layout with files named `summary.json`,
in the sibling folders). A partial with no final, a partial newer than the final, or a final that says `exit.clean: false` (a teardown hang) is recorded as
`previous_exit: {clean: false, last_partial: {…}}` and logged as the WARN `regia::phase`
event `previous_session_unclean`.

Every section is optional; a field that is unknown is absent.

| Section | Content |
|---|---|
| `regia`, `system`, `display`, `settings` | Build; OS, CPU, RAM, GPU, driver, backend, VRAM; window size, mode, present mode, HDR, `monitors[]` (every monitor, the one in use marked `current`); preset, render scale, GI, `upscaler` (the one actually used), `auto_plan` (the first-launch choice, ADR-157). |
| `host` | Free-form, from `TelemetryHost`: `set(key, string)` and `set_value(key, json)`. Recommended keys: `app`, `version`, `git`, `build`, `data_tier`, `network`. Read again at every partial and at the exit. |
| `session` | `seconds`, `frames`, `maps` (the host's map names). |
| `frames`, `frames_by_map` | p50/p95/p99, `over_33ms_pct`, `over_50ms_pct`, `bound` over the session; the same per map, from the host's markers. |
| `hitches`, `stalls` | Count, suppressed, `top_causes` (`pipeline_create`, `blocking_poll`, `inline_bake`, `gpu_resource_create`, or the frame's most expensive span); frames of 1 s or more with the phase in effect. |
| `memory`, `shaders`, `audio`, `errors` | VRAM and RSS peaks (RSS is unknown on Windows), `texture_budget_exceeded`; pipelines, startup warm time, cache, `backend`, `driver_cache` (`on`/`off`/`unknown`); audio suspends; GPU errors, device losses, surface errors, panics. |
| `exit`, `previous_exit` | `clean`, `ms`, `watchdog`, `steps`; the previous session's. |

`cargo xtask alpha-triage <dir>` reads each session's summary first (a summary with no log beside it
is a session row of its own), then the JSONL for detail, and prints the per-map frame table, stalls,
the phase timeline and `previous_exit`.

## Frame profiler

`DefaultPlugins` creates the process frame profiler and `regia::run`/`run_with` install it as a
`tracing` layer, so the hitch events name their top CPU spans and the breakdown has per-system
and host sections. A host that installs its own subscriber must add the layer
(`.with(regia::frame_profiler())`); after 600 frames without any span (checked from frame 120, once 60 frames have been profiled) the engine logs one WARN on
`regia::telemetry` saying so. Until the host calls
`regia_core::profiling::set_host_span_prefixes`, host sections are spans whose target starts with
`do27::` or with the executable's name as a crate path (`arena` → `arena::`; `do27.exe` → `do27::`).
A name that is empty, starts with `regia`, or starts with a reserved crate name (`wgpu`, `bevy`,
`tracing`, and the others named on `default_host_span_prefix`) is not used. A successful call
replaces both of those defaults and can be made only once.

## Feeding telemetry from engine code

Code below the telemetry crate (assets, render, terrain, VFX) never logs per frame. It uses the
taps in `regia_core::telemetry`:

- counters (`add(Counter::…, n)`);
- gauges (`set_gauge(Gauge::…, v)`);
- `note(NoteKind::…, label, value)`, which copies into a fixed 64-slot ring and drops and counts
  when the ring is full;
- `SlowLoad::start(kind, &id)`, a guard that notes a load slower than 100 ms.

The plugin drains the ring once per frame.

## Agent

`telemetry.get_last_summary {session?, session_summary?}` returns the newest summary, the header
(with `session: true`), the `summary.json` record as of now (with `session_summary: true`) and the
counts. See [Telemetry (agent)](../agents/telemetry.md).

## Renderer sample freshness and hidden waits (v0.5.1)

A renderer may retain its last timing for many update-only ticks. Telemetry consumes each CPU
and GPU publication once. `FrameCpuTimings` and `GpuTimings` expose `frame_id` (the renderer
frame that produced the timing) and `sample_id` (a process-wide publication identity). GPU
readback preserves its submitted frame ID; it does not become the frame that read it back.
Renderer replacement cannot collide merely by restarting its frame counter.

The existing numeric `live.cpu_ms` and `live.gpu_ms` are zero when no fresh publication exists.
Hitch records include optional `render_timing` metadata from their observation. At delayed
emission, `gpu_frame_id` names the available readback; `gpu_ms` and `top_gpu_passes` are
absent unless it matches that hitch's fresh CPU render frame. Two update-only ticks do not
advance the renderer's readback latency or make its cached GPU sample current.
Use the optional `live.cpu_sample_ms` and `live.gpu_sample_ms` for explicit nulls. The optional
`live.timing` object provides `cpu_frame_id`, `gpu_frame_id`, `cpu_fresh`, `gpu_fresh`,
`cpu_absent`, and `gpu_absent`. A stale value retains its source frame ID, but contributes no
new duration to a histogram or GPU-pass mean. Absence reasons are:

| Reason | Meaning |
|---|---|
| `hidden` | The runner reports occlusion, stalled redraw delivery, or repeated surface refusal. |
| `no_scene` | Extraction has no camera view, or scene warming temporarily owns the renderer. |
| `renderer_idle` | No newer timing publication is available on this tick. |
| `no_renderer` | No runtime renderer is installed. |
| `no_gpu_timing` | GPU timestamps are disabled or unsupported. |
| `pending` | A fresh CPU render has no new GPU readback yet. |

Summary `cpu_ms.samples` and `gpu_ms.samples` count positive-duration publications in that
window. For example, one 4218.53 ms render followed by eight update-only ticks contributes one
CPU sample, not nine. The window's CPU mean is still 4218.53 ms; its sample count and the stale
ticks explain why it is not the average CPU cost of all nine updates. A summary reset does not
make cached samples fresh again. GPU-pass aggregates follow GPU publication freshness too.
Draw and shadow-CPU aggregates advance only with fresh CPU publications, so idle ticks do not
repeat the renderer's cached statistics.

`frame_breakdown_ms.event_wait` is time actually spent asleep in the window event loop, measured
from winit's wait-start timestamp to its wake callback. It excludes event handlers and update
work. `outside_update` is the remaining between-update time after host schedules, pacing and
this wait are removed. Every phase contributes to total wall time exactly once. Hitch
`interval_ms.event_wait_ms` is the wait belonging to that frame interval; a dominant measured
wait has cause `event_wait`. `event_loop_ms` still includes event sleep and other event-loop time.

`live.event_wait_ms` refers to the last completed app frame, identified by `live.phases_frame`;
this is one update behind the live tick. `live.runner` reports the current runner state:
`focused`, `occluded`, `stalled`, `surface_refused`, and `throttle_ms`. The same optional runner
object in a phase breakdown belongs to that completed phase record. `throttle_ms` is the
configured hidden/frozen/idle period, not a claim that every tick was throttled. The visibility
flags distinguish a hidden wait from a normal redraw wait. Missing `FramePhases` or `RunnerState`
produces null for the corresponding diagnostics. An App-based custom runner must call
`App::record_event_wait` to measure its sleep; otherwise that phase stays zero and the sleep
remains in `outside_update`.

The hidden deadline is measured from frame start: a 25 ms update at the default 100 ms period
leaves 75 ms until the deadline. Early input wakeups do not grant extra simulation steps.
An update that already exceeds the period adds no further artificial wait. Hidden throttling
and the focused/fullscreen recovery grace remain enabled.

The runner starts `focused = false` and changes it only from OS focus events. A host visibility
guard may start `focused = true` as an optimistic policy for a capture that has not received an
OS event. These fields have different meanings: the guard default is not evidence of OS focus.
Use `live.runner.focused` for the engine's event-driven state; this change does not modify host
visibility policies.

`frame_bound` requires fresh CPU and GPU publications, a visible runner, and event sleep below
half the interval (`EVENT_WAIT_SHARE = 0.5`). Small normal redraw waits remain classifiable. Otherwise it
is `unknown`, with `hidden`, `renderer_idle`, or `event_wait` added to its existing reasons.
The summary ratio uses only eligible pairs, so a cached GPU duration cannot classify an idle
window. Delayed GPU timing is still an approximation of load; `cpu_bound` is not CPU utilization.
The optional JSON additions keep telemetry schema 1. Feedback's optional `render_timing` metadata
uses the same freshness fields without changing its CSV or native crash wire record.
