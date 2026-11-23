# Telemetry (`telemetry.get_last_summary`)

The engine emits structured `tracing` events, all with targets under `regia::*`: the session
header, frame summaries, hitches, GPU errors, render problems and streaming events. They are
documented field by field in [Telemetry](../user-guide/telemetry.md). An agent reads the newest
summary without parsing log files:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "telemetry.get_last_summary", "params": { "session": true } }
```

<!-- not-a-request -->
```json
{ "summary": { "seq": 3, "reason": "periodic", "frames": 1187, "window_s": 10.0,
               "frame_ms": { "p50": 8.25, "p95": 9.5, "p99": 12.5, "max": 31.0, "mean": 8.4 },
               "fps_histogram": { "ge_120": 1150, "ge_90": 30, "ge_60": 7, "…": 0 },
               "cpu_ms": { "mean": 2.1, "p95": 2.6, "max": 6.0 }, "gpu_ms": { "mean": 7.6, "p95": 8.9, "max": 14.2 },
               "bound": "gpu", "gpu_pass_ms": { "forward": { "mean": 2.4, "max": 3.1 }, "…": {} },
               "textures": { "resident_bytes": 1610612736, "budget_bytes": 2147483648, "over_budget": false, "queue_depth": 0 },
               "memory": { "rss_bytes": 3221225472, "vram_allocated_bytes": 4294967296 },
               "hitches": 1, "created_after_warmup": { "pipelines": 0, "…": 0 } },
  "session": { "adapter": { "name": "NVIDIA GeForce RTX 5080", "backend": "Vulkan" }, "fallbacks": [], "…": {} },
  "counts": { "frames": 3561, "summaries": 3, "hitches": 1, "hitch_events": 1, "problems_events": 0,
              "stream_events": 0, "surface_events": 0, "suppressed": 0 },
  "level": "verbose", "summary_interval_s": 10.0 }
```

- `summary` is null for the first 10 s of a session. `session` is null until the first frame
  renders, or when the `session` parameter is not set.
- An error with code `F0003` means telemetry is off: `REGIA_TELEMETRY=off`, or the app has no
  `TelemetryPlugin`.
- `level` is `normal`, `verbose` or `ship` (v0.5.1, ADR-8417; `REGIA_TELEMETRY=ship`). At `ship`,
  `session` is the allowlisted player-build header (adapter class and vendor family, RAM and VRAM
  tiers, no names or ids), `counts.hitch_events` stays 0 and `session_summary` has no `host`
  section. The field list is in [Ship level](../user-guide/telemetry.md#ship-level).

Typical uses:

- After a scripted run (`play.wait_until`), assert `summary.frame_ms.p99` and
  `summary.hitches`.
- Check `summary.created_after_warmup.pipelines == 0`: no shader compiled mid-game.
- Check `summary.frame_scratch.growths_after_warmup == 0`: the renderer's per-frame lists stopped
  growing ([Frame list growth](frame-scratch.md), v0.1.28).
- Read `session.fallbacks` to see why a machine looks different.
- `session_summary: true` adds the `summary.json` record as of now (frames per map, stalls, hitch
  causes, memory peaks, errors, exit state) and `summary_paths` says where the files go.

## The newest frame (`telemetry.live`, v0.1.14)

`telemetry.live` returns what telemetry saw for the newest frame, without waiting for a summary:

```json
{ "jsonrpc": "2.0", "id": 2, "method": "telemetry.live" }
```

```json
{ "live": { "frame": 5012, "frame_ms": 59.1, "cpu_ms": 11.8, "gpu_ms": 30.2, "bound": "cpu_bound",
            "render_scale_permille": 670, "wait_acquire_ms": 0.4, "wait_present_ms": 0.2,
            "wait_readback_ms": 0.0 },
  "thresholds": { "gpu_bound_share": 0.9, "cpu_bound_share": 0.75 } }
```

- `cpu_ms` is the renderer's `render_into` only (cull, record, submit), not the frame.
- `bound` compares `gpu_ms` with `frame_ms`: `gpu_bound` at a share of 0.9 or more, `cpu_bound`
  at 0.75 or less, `balanced` in between, `unknown` without GPU timing. `cpu_bound` means a lower
  render scale will not raise the frame rate.
- `render_scale_permille` is what the renderer drew (internal width × 1000 / output width),
  null before it drew.
- `wait_*_ms` are the main thread's waits on the GPU in the last window frame: swapchain acquire,
  present, and screenshot readbacks.
- `held` is true when the frame showed the paused-scene hold's kept image (ADR-7890): no scene pass
  ran and `gpu_ms` is the composite plus the UI. Such frames are not in the summary's `frames` or
  frame-time figures (its `held` section counts them) and never feed the hitch median; a governor
  should skip them. See [Frame pacing](frame-pacing.md).

The summary carries the same signals over its window: `frame_bound`, `render_scale_permille`,
`wait_acquire_ms`, `wait_present_ms`, `wait_readback_ms`, `cpu_wait_on_gpu_ms` and `live`. A
steady slow frame rate is no longer a hitch (v0.1.14): `summary.hitches` counts spikes out of
the rolling median only.

For one-off hitch attribution, use `play.hitch_report`. Telemetry is the always-on version that
ships in player builds.

## Hitch truth (v0.5.1, ADR-7898)

A DO27 tester round showed three ways the hitch log misled triage. Since v0.5.1:

- **Swapchain waits are not hitches.** A frame that passes the hitch tests but spent at least half
  of its excess over the median in this frame's acquire + present wait (and is at most
  `hitch_abs_ms` over the median) counts as a present wait: `counts.present_waits` in
  `telemetry.get_last_summary` and `summary.present_waits`, not `hitches`.
- **The pacing floor.** Under a frame cap or vsync, a frame shorter than 1.5 pacing periods is never
  a hitch. `telemetry.live` returns the floor in effect:

```json
{ "jsonrpc": "2.0", "id": 3, "method": "telemetry.live" }
```

<!-- not-a-request -->
```json
{ "live": { "frame": 51, "frame_ms": 736.4, "cpu_ms": 0.0, "gpu_ms": 0.0, "bound": "unknown",
            "bound_reason": "no_scene", "render_scale_permille": null, "wait_acquire_ms": 0.04,
            "wait_present_ms": 0.26, "wait_readback_ms": 0.0 },
  "thresholds": { "gpu_bound_share": 0.9, "cpu_bound_share": 0.75, "hitch_floor_ms": 8.0,
                  "pacing_period_ms": 0.0, "present_wait_share": 0.5 } }
```

- **`bound: unknown` says why.** `live.bound_reason` and the summary's `bound_reason` are
  `no_gpu_timing`, `loading`, `no_scene`, `no_gpu_sample`, `hidden`, `renderer_idle`, or
  `event_wait` (null when the bound is known).
- **The hitch cause is the frame thread's.** The `regia::hitch` record's `cause` comes from the
  main and render threads only; worker spans are listed as `background_spans`. The record adds
  `unaccounted_ms`: the part of the frame interval inside no phase the engine measures. When that
  is at least half the frame the cause is `outside_frame`: the process was in the OS, a window
  change or a driver call outside the event loop, or not scheduled. `window_events` lists the
  resizes, focus and mode changes of the interval. Fields: [Telemetry](../user-guide/telemetry.md),
  `regia::hitch`.

Typical checks: `summary.hitches` with `summary.present_waits` beside it (a vsync-bound game has
present waits, not hitches); a launch hitch whose `cause` is `outside_frame` with
`window_events.mode_changes` 1 is a fullscreen switch, not engine work.


## Stale render samples and event waits (v0.5.1)

Read `telemetry.live` after a hidden or loading interval. `live.timing` retains the source CPU/GPU
render frame IDs and reports freshness separately. `cpu_sample_ms` and `gpu_sample_ms` are null
when no fresh publication exists; the older numeric fields are zero. Inspect `cpu_absent` and
`gpu_absent` for `hidden`, `no_scene`, `renderer_idle`, `no_renderer`, `no_gpu_timing`, or `pending`.
Do not multiply the last valid renderer time by the number of update ticks.

`live.runner` reports the runner's `focused`, `occluded`, `stalled`, `surface_refused`, and
`throttle_ms`. Focus starts false until the OS reports it. `live.event_wait_ms` and
`live.phases_frame` identify the measured wait of the last completed app frame; runner state in
the live response is current. A hidden 25 ms update at a 100 ms period leaves 75 ms for waiting.
`telemetry.get_last_summary` exposes `cpu_ms.samples`, `gpu_ms.samples`, and the separate
`frame_breakdown_ms.event_wait` phase. `frame_bound.unknown_reasons` includes `hidden`,
`renderer_idle`, and `event_wait`. A dominant measured wait is a hitch cause named `event_wait`.
`thresholds.event_wait_share` is 0.5: a measured wait taking at least half the interval makes
its bound unknown; small normal redraw waits remain classifiable. These are optional additions;
tolerate their absence in older logs. No new method or error code.

Hitch records add `render_timing` and `gpu_frame_id`. A delayed hitch's `gpu_ms` is omitted when
the available readback does not match its fresh render; `top_gpu_passes` is absent then.
Elapsed update ticks alone do not establish GPU sample freshness.
