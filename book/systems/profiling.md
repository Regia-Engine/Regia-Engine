# Profiling and performance tools

Regia Engine treats frame time as a feature: per-frame engine work carries `tracing` spans, the GPU
passes are timed with timestamp queries, frame statistics are always available to the game and
the agent protocol, and soak runs check budgets that include a **hitch rule** (no frame over
50 ms after the first ten seconds of play). This chapter shows where each number comes from.

## Frame statistics in the game

`DefaultPlugins` installs `DiagnosticsPlugin`, which keeps the `Diagnostics` resource up to date:

```rust
use regia::prelude::*;

/// Logs frame statistics and the five most expensive CPU spans every ten seconds at 60 fps.
fn frame_report(time: Res<Time>, diagnostics: Res<Diagnostics>, profiler: Option<Res<Profiler>>) {
    if time.frame_count() % 600 != 0 {
        return;
    }
    info!(
        "{:.1} fps (1% low {:.1}), {:.2} ms avg, {} entities, {} draw calls",
        diagnostics.fps,
        diagnostics.fps_1pct_low,
        diagnostics.frame_time_avg.as_secs_f64() * 1000.0,
        diagnostics.entity_count,
        diagnostics.draw_calls,
    );
    if let Some(profiler) = profiler {
        for (span, cost) in profiler.0.top(5) {
            info!("  {span}: {:.3} ms", cost.as_secs_f64() * 1000.0);
        }
    }
}
```

- `Diagnostics`: fps, average frame time, the 1 % low, entity count, draw calls, and allocated
  bytes when a counting allocator is installed.
- `Profiler` (from `regia::run`): the process-wide frame profiler, fed by the logging layer. It
  keeps per-span-name CPU time for the recent frames: `last_frame()`, `averages(n)`, `top(n)`.
  Spans are free when nobody records them, so instrument your own expensive systems with
  `#[tracing::instrument]` or `info_span!` the same way the engine does.

The agent protocol's `play.get_diagnostics` returns the same numbers, which is how scripts, CI and
soak runs read them from a running game.

## Whole-run span reports in benches

`Profiler` keeps only the recent frames. To see where a whole bench run's CPU time went, wrap a
profiler in `regia_core::profiling::RunTotals`: call `end_frame()` once per measured frame,
`skip_frame()` for warm-up frames, and print `report(n)`. The table lists the `n` span names and
ECS systems with the most **self time** (the span's time minus the child spans entered inside it on
the same thread), with their inclusive time and calls per frame. Times are means per frame summed
over every thread, so a span that runs on four workers at once can exceed the frame time. Work a
span hands to another thread and then waits for counts as the waiting span's self time.

The engine's headless benches print it when `REGIA_SPAN_REPORT` is set (the value is the row
count; any other value prints 30 rows):

```console
$ REGIA_SPAN_REPORT=25 cargo run --release -p arena -- --bench
$ REGIA_SPAN_REPORT=25 cargo run --release -p render_lab -- sponza_1080p_runtime_fps
$ REGIA_SPAN_REPORT=25 cargo run --release -p terrain_lab -- --do27-chase --variants base
```

In your own bench, call `RunTotals::install_from_env()` first thing in `main`: it starts logging
with the profiler installed and returns `None` when the variable is unset. A process that also
runs the telemetry plugin uses `RunTotals::spans_only`, because both would drain the same
per-system counters. The engine's own hotspot history is in `docs/perf-log.md`.

## Flame graph

`Profiler`'s flat totals lose the call hierarchy. Turn on flame capture and read the span call tree
back over a window of frames:

```rust
use regia::prelude::*;
use regia::core::profiling::FlameGraph;

fn flame_graph(profiler: Res<Profiler>, time: Res<Time>) {
    profiler.0.set_flame_enabled(true); // once, at startup
    if time.frame_count() % 600 == 0 {
        let graph: FlameGraph = profiler.0.flame_graph(120); // mean over the last 120 frames
        info!("{}", graph.folded_stacks());                    // grep-friendly `name;name  <self ms>`
        std::fs::write("frame.svg", graph.to_svg()).ok();      // capture to file
    }
}
```

`flame_graph` returns the engine's span call tree (the `scope!` spans and host sections), each node
with `inclusive`, `self_time`, `calls` and its `children`; `folded_stacks` and `to_svg` are the text and
SVG forms. A span whose nearest timed ancestor is a system or schedule span appears as a root; ECS
systems themselves stay on the flat per-system table (`top`, `take_system_times`). Capture keeps at most
65536 edges per frame; further exits in that frame are dropped and do not allocate a call path. Leave
capture on only while you need it.

## Work on other threads

Every ECS system runs inside its own span (bevy's `trace` feature), but a span covers only the
thread it was entered on. Work a system hands to a pool or a worker thread opens its own span there,
so it is counted too:

| Span | Where it runs | What it covers |
|---|---|---|
| `par_chunk` | compute pool | one chunk of `regia_core::tasks::par_chunks` / `par_chunks_mut`; `of` names the caller's span, `index` the chunk |
| `async_task` | async compute pool | a future started with `regia_core::tasks::spawn`, on every poll; `of` names the caller's span |
| `io_task` | IO pool | a future started with `regia_core::tasks::spawn_io`, on every poll; `of` names the caller's span |
| `scatter_collect_tiles` | compute pool | one tile range of the scatter cull (`collect`) |
| `finish_segment` | `regia-encode-*` workers (or the render thread) | `CommandEncoder::finish` of one frame segment |
| `finish_join` | render thread | the frame's last `finish` plus the wait for the workers' segments |
| `collect_gpu_timings` | render thread | reading back the timestamp queries of older frames |

Each is a child of the span that was current where the work was handed off, so Tracy and the
`of` field tie it to the system that asked for it. Their times add up over every thread, like
every other span in the tables above. Your own `par_chunks` calls get `par_chunk` spans without any
code; in a `regia_core::tasks::scope`, open a span inside each spawned task (create it before the
spawn and use `span.in_scope(..)`, as the scatter cull does). ADR-1378 records the audit that added
them.

## GPU timings

The renderer times each pass with timestamp queries when the device supports them.
`FrameRenderer::gpu_timings()` (reach it through the `RuntimeRenderer` resource) returns
`GpuTimings`: shadows (and each sun cascade and the local-light atlas separately), light clusters,
depth prepass, GPU culling, SSAO, forward, screen-space GI, SSR, atmosphere, clouds, froxel fog,
post, and the terrain's own share of the forward pass. `report()` formats them as one line:

```rust
use regia::prelude::*;
use regia_render::RuntimeRenderer;

fn gpu_report(time: Res<Time>, renderer: Res<RuntimeRenderer>) {
    if time.frame_count() % 600 == 0 {
        info!("gpu: {}", renderer.0.gpu_timings().report());
    }
}
```

On the window path the values are at least two frames old (the renderer never waits on the
query). Measure with the GPU busy: an idle GPU clocks down and every pass reads longer.

## Tracy

`regia_core` has a `tracy` feature that streams every span to the
[Tracy](https://github.com/wolfpld/tracy) profiler. Enable it in your launcher's `Cargo.toml`
(add `regia_core` with `features = ["tracy"]`), start Tracy, and run the game: CPU spans from
every thread appear on its timeline.

## Picking the GPU

On machines with several adapters, `REGIA_GPU_UUID`, `REGIA_GPU_PCI`, `REGIA_GPU_INDEX` or
`REGIA_GPU_NAME` choose the card for every device the engine creates, and `REGIA_GPU_STRICT=1`
makes an unmatched request an error instead of a fallback.
[Performance and benches](../user-guide/perf-benches.md) has the details and the adapter list.

## Soak runs and the hitch rule

`cargo xtask soak` runs a built game for minutes at a time, samples it through the agent protocol,
optionally drives it with an input script, and checks the result against a budgets file:

```console
$ cargo build --release -p arena
$ cargo xtask soak --exe target/release/arena --minutes 5 --project-dir samples/arena \
    --input budgets/arena-soak.input.ron -- --bots 4
```

`budgets/default.ron` is the desktop reference budget:

| Budget | Default |
|---|---|
| `frame_p99_ms` | 6.9 ms |
| `frame_max_ms` (the hitch rule) | 50 ms, after `warmup_secs` |
| `warmup_secs` | 10 s |
| `rss_mb` | 1536 MB peak |
| `start_secs` (cold start to first agent answer) | 3 s |
| `errors`, `crashes` | 0 |
| `warns` | 2 |

The soak derives an unclamped frame-time series from the frame counter and the wall clock, so a
frame-rate cap cannot hide a hitch. `--sessions K` repeats the run; reports go to
`docs/evidence/m18/` by default; `cargo xtask budgets check` re-checks a written report.

## Hitch reports

Zero hitches is a hard gate (`docs/specs/27-renderer-excellence.md` § 2.4): after warm-up no
pipeline or bind group is created on the frame path, no frame is over 50 ms and the frame-time
percentiles stay close to the median. The `play.hitch_report` agent method measures a running
app against it:

```json
{"jsonrpc":"2.0","id":1,"method":"play.hitch_report","params":{"seconds":60,"warmup_s":2}}
```

The reply comes after `warmup_s + seconds`, with the frame-interval percentiles (`p50_ms`,
`p99_ms`, `p999_ms`), the hitch counts (`over_2x_p50`, `over_50ms`, `longest_ms`), the renderer's
device calls after warm-up (pipelines, bind groups, buffers, textures, uploads, blocking polls),
the worst spikes, and a `pass` verdict with every missed row in words. The usual fix for creates
on the frame path is `FrameRenderer::prewarm` at level load. The causes, their fixes and the
look-dev soak that drives whole maps are in the agent guide's [Hitches](../agents/hitches.md)
page.

## Benches

| Tool | What it measures |
|---|---|
| `render_lab` (`samples/render_lab`) | Reference render scenes; `render_lab outdoor` prints atmosphere, cloud and froxel-fog pass times on a Desktop High frame |
| `samples/lookdev` (`look_r2`, `default_look`, `do27_chase`) | The look-dev shots with measurements; `do27_chase` replays a vehicle chase on *Direct Order 27*'s content and prints wall, CPU and per-pass GPU p50/p99 |
| `arena --bench` | The arena's bot simulation without rendering |
| `cargo xtask build-times` | Clean and incremental build times against the workspace budgets |

[Performance and benches](../user-guide/perf-benches.md) covers the DO27 frame bench in detail.

## Budgets to keep in mind

- **Hot paths do not allocate.** Engine systems reuse buffers frame to frame; do the same in
  yours (keep a `Local<Vec<_>>`).
- **Per-frame work over 0.1 ms gets a span**, so it shows up in `Profiler` and Tracy.
- **Pipelines are built before gameplay.** Material graphs and new render paths compile on first
  use; render a warm-up frame behind a loading screen so the first real frame does not hitch.
